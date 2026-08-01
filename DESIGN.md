# vellum 设计说明

本文件是 `ARCHITECTURE.md`（需求契约）的实现侧回答：模块怎么切、每个技术选型的备选与放弃理由、进程模型、长截图算法要点。
性能目标、测量方法与实测数据在 `PERFORMANCE.md`。

对应 `ARCHITECTURE.md` §3 与 §8.1。

## 1. 进程模型

四个可执行文件，边界按「谁付启动开销」划分，而不是按功能划分。

| 二进制 | 链接 GTK | 角色 |
| --- | --- | --- |
| `vellumctl` | 否 | 快捷键唯一入口。手工解析 argv，只认 `region`/`long`/`pin-last`，发一条 JSON 到 socket 就退出 |
| `vellum` | 否 | 完整 CLI（含 `daemon` 子命令）。`status`/`doctor`/`logs`/`restart`/`shortcuts`/`tray` |
| `vellum-ui` | 是 | 唯一的 GTK 进程：选区 overlay、标注、长截图面板、钉图窗口、OCR/翻译结果窗 |
| `vellum-tray` | 否 | 托盘。只走 D-Bus（SNI + `com.canonical.dbusmenu`） |

**为什么不做成一个二进制**：GTK 的动态链接与初始化在本机实测占端到端延迟的约三分之二（`PERFORMANCE.md` §2）。快捷键路径每次按键都要付这笔账，而它 99% 的情况下只是把一条 20 字节的 JSON 递给已经在跑的守护进程。`vellumctl` 不链接 GTK、不链接 clap、不链接 regex。

**交接用 `execv` 而不是 `spawn`**：守护进程记录它启动的子进程 pid，并向该 pid 发 `SIGUSR1` 结束长截图。若 `vellum` 用 spawn 再退出，pid 就断了。`execv` 原地替换镜像，pid 跨交接存活。

**守护进程住在 `vellum` 里**（`vellum daemon`），不是第五个二进制：`client::spawn_daemon()` 直接 re-exec `current_exe()`，只有一条路径要保持同步。

**动作生命周期单点串行化**：完成、启动与 shutdown 共用一把 lifecycle mutex。子进程退出时必须依次更新事件、恢复长截图光标、记录日志并按需通知；不能由 `snapshot()` 只清除 busy 标志。shutdown 先关闭接单，再恢复长截图光标、杀死当前动作并做有界回收；极慢退出会移交后台 reaper，避免关停无限等待。通知、托盘直启和无 systemd 回退启动的进程同样交给后台 reaper，长驻父进程不丢弃 `Child`。

### 命名空间隔离

vellum 使用独立且稳定的命名空间：socket/lock 在 `$XDG_RUNTIME_DIR/vellum/`（可用 `VELLUM_RUNTIME_DIR` 覆盖）、unit 名 `vellum.service`/`vellum-tray.service`、配置 `~/.config/vellum/config.toml`。它不复用旧 Python `pngshot` 的运行期路径，避免升级或迁移时发生 socket 与服务冲突；`paths.rs` 有断言防止未来误合并。

`~/.config/pngshot/config.toml` 作为**只读**回退（用户已有配置可直接生效），vellum 永不写入它。

## 2. crate 划分

7 个 workspace 成员。切分依据是「能不能在没有图形会话的情况下测试」。

```
vellum-core    无依赖的地基：Rgb8、Rect、config、paths、grim 封装、剪贴板、通知、带超时的子进程、合成器抽象
vellum-stitch  长截图算法。纯计算，无 IO 无 GTK，可在 CI 里跑合成回归
vellum-text    OCR + 翻译。纯计算 + HTTP/子进程，无 GTK（结果窗在 worker 线程调它）
vellum-ipc     协议 + 客户端 + 守护进程。无 GTK
vellum-cli     `vellum` / `vellumctl` 两个 bin + doctor + 快捷键管理（niri KDL / Hyprland）
vellum-ui      唯一链接 GTK 的 crate
vellum-tray    托盘，只依赖 core + ipc + ksni
```

`vellum-stitch` 与 `vellum-text` 不依赖 `vellum-ipc` 或任何 UI 代码，所以 238 个 Rust 测试里绝大多数不需要 Wayland 会话。

合成器抽象（`vellum-core/src/compositor/`）放在 core 而不是 GTK 二进制里，因为 `doctor` 也要报告检测到的合成器 —— 一份实现不会漂移，两份会。

## 3. 技术选型

每项都记了备选与放弃理由，避免后人重新走一遍。

### 图像与拼接：手写，不用 OpenCV

Python 版用 numpy + OpenCV。Rust 侧改为手写：

- **放弃 `opencv` crate**：拖入整个 C++ 运行时与 bindgen/clang 构建依赖，而实际需要的只是形态学 CLOSE、`divide`、Otsu、CLAHE 和一个 3×3 主成分投影；这些小算子手写后仍然比引入整套 OpenCV 更容易审计。
- **放弃 `ndarray`**：拼接热路径是「按行取切片做定长比较」，`Vec<u8>` + 手写 stride 已经足够，多一层抽象换不到可读性。
- 保留 `image` 0.25（仅 `png` feature）做 PNG 编解码与 Lanczos 缩放，`rayon` 做行级并行。

形态学基线在 `vellum-text/src/prep.rs` 里按 OpenCV 语义重实现（椭圆核按 `getStructuringElement(MORPH_ELLIPSE)` 的半宽公式生成，并分解为逐 dy 的滑动窗口 row-max/row-min，避免 25×25 核的 625 ops/px）。其上增加按需渲染的 CLAHE、相反文字极性和 RGB 主成分/最大通道候选，用于暗淡字色、明暗渐变和等亮异色文字；实测见 `PERFORMANCE.md` §4。

### 截屏：`grim` 子进程 + PPM

- **改动**：Python 版用 `grim -t png`，vellum 用 `grim -t ppm`。实测全屏 22 ms vs 121 ms，每帧省约 99 ms（`PERFORMANCE.md` §4）。PNG 编码在这里纯属浪费——图刚出来就要解码回像素。PPM 由手写的 P6 解析器读取（跳空白与 `#` 注释，要求 maxval 255）。
- **暂未改为直连 `wlr-screencopy`**（`ARCHITECTURE.md` §4.1 要求评估）：实测区域抓帧 16 ms，其中进程创建只占很小一部分，而拼接一帧只要 0.89 ms——采样率的瓶颈是合成器交付一帧的时间，不是 grim 的开销。直连 screencopy 需要自己管理 wl_registry/buffer 生命周期与多输出/缩放，换来的余量有限。结论记在这里而不是当作待办：**有数据支持暂不做**，若将来要提高采样率再重新测量。

### GTK 绑定版本必须整组锁定

`gtk4` 0.11.4、`gtk4-layer-shell` 0.8.0、`gdk4` 0.11.4、`glib` 0.22.8、`cairo-rs` 0.22.0、`pango` 0.22.8、`pangocairo` 0.22.8。

**踩过的坑**：默认解析会给 `pangocairo` 选 0.21.2，它拖入 `cairo-rs` 0.21.5，于是 `cairo-rs` 出现两个版本。两套 `ImageSurface` 是不同类型，pango 画出来的 surface 传不进 gtk。所以七个版本在根 `Cargo.toml` 里全部 `=` 固定，并附注释说明必须同步移动。

`gtk4` 只开到 `v4_12` feature（本机是 GTK 4.22）：需要 `CssProvider::load_from_string`（4.12 起，`load_from_data` 已废弃），但不想把最低 GTK 要求抬得更高。

### 文字渲染必须走 Pango

cairo 的 toy font API 无法 shape CJK。工具栏、尺寸提示、标注文字全部经 `pangocairo`。这不是审美选择，用 toy API 会直接得到豆腐块。

### 托盘：`ksni` 0.3.6

- **放弃 `libappindicator`/`ayatana` 绑定**：要拖 GTK 3 进来，而托盘进程本该是最轻的一个。
- **选中理由**：ksni 从同一个对象同时导出 `org.kde.StatusNotifierItem` 与 `com.canonical.dbusmenu`。传统 dbusmenu 是硬约束——本机宿主（niri/Hyprland 下的 QuickShell）读的是它，只答现代接口的表现是「图标出现但菜单为空」。已用 `GetLayout` 实测拿到完整 13 项菜单树。
- 开 `blocking` feature 保留默认 tokio runtime，托盘代码里不出现 async main。

### OCR：`tesseract` 子进程，不用 `leptess`

- **放弃 `leptess`/`tesseract-sys`**：本机 leptonica 的 soname 是 `/usr/lib/libleptonica.so`，而 `tesseract-sys` 期望 `liblept.so.5`；另需 bindgen/clang 构建依赖。
- **子进程可接受**：OCR 已在 worker 线程，结果窗口先开占位（「识别中…」），进程创建不阻塞界面。普通干净截图只跑基线；只有低置信度/低对比/异色场景才按需增加候选。
- **质量选择不用“字越多越好”**：Tesseract 输出 TSV 置信度，vellum 按行重建文字、剔除稀疏模式找到的弱彩色边缘噪声，并在混合语言顺序之间逐行融合。所有候选共享 30 秒总时限，不能把一次 OCR 放大成多次 30 秒等待。
- **管道必须并行排空**：PNG stdin、TSV stdout 与 stderr 同时读写；否则任一管道超过内核容量时，子进程和父进程会互相等待并被误报为超时。外部命令单独建立进程组，超时时整组终止，避免 fork 后代继承 pipe 令读取线程永久卡住。

### HTTP：`ureq` 3.3.0

同步阻塞 API，正好配合「worker 线程 + `glib::idle_add` 回填」的结构。**放弃 `reqwest`**：会拖入 tokio，而这里没有任何需要 async 的并发。注意 ureq 默认无超时，所有调用点都显式设了（健康探测 200 ms、清理 1 s、翻译取 `timeout_s`）。

### 其他

- **不用 `criterion`**：用户等的是「松手到出图」的一次墙钟时间，采样型 harness 只报 per-`add` 吞吐，而尾部延迟住在 offline rebuild 与最后一次 `vstack` 里。两个 bench 都是 `harness = false` 的普通程序。
- **不加 `tempfile` dev-dependency**：测试里各自用 `std::env::temp_dir()` + 计数器/pid/纳秒时间戳自建临时目录，`Drop` 里清理。
- **fixture 不用 `rand`**：合成页面用确定性混淆器生成，保证跨机可复现。

## 4. 长截图算法要点

完整常量与推导在代码注释里，这里只记结构与「为什么不能换成别的」。

### 匹配：行签名 + 稀疏 RGB 校验

每帧取 96 个采样列（`min(width, 96)`，索引均匀分布），每行压成 3 个浮点特征（亮度均值、对比度、边缘能量）。候选偏移按「上次偏移优先，然后向两侧扇出」枚举，先用行签名打分，通过后再用稀疏 RGB 做像素级校验。

**不能退回模板匹配**：`cv2.matchTemplate` 在抗锯齿文字上给出假低分，这是原实现踩过的坑。

分数超阈值时走一次 robust 路径：`trimmed_mean` 只保留最好的 80% 重叠行，用来吸收局部动画。**这条路径有明确能力边界**：损坏行数超过重叠行的 1/5 就会失效。20 px 步长下实测 H=16 的动画块（16.4% 损坏）仍正确，H=20（18.2%）开始出错。回归测试用 H=16 并把这段推导写在注释里，避免有人把用例改大之后误判为回归。

### canvas 增量块拼接

新内容按块 append/prepend 到 canvas，**只在 `result()` 时合并一次**。逐帧 vstack 会让长图越滚越慢（每帧重新分配并拷贝整张图）。

### 关键帧 + 离线重建

在线拼接容错优先；`result()` 时用压缩关键帧（zlib level 1，硬上限 48 MiB / 160 帧）跑一次最短路径 DP 重建，把在线阶段被迫接受的坏链接换掉。淘汰关键帧时按 `(reason 优先级, span)` 排序，优先丢普通 motion 帧，保住 turn/recovered/failure 这些信息量大的。

融合时按「视口中心权重高」的三角权重混合重叠行；像素差超过 `FUSION_MAX_PIXEL_DELTA` 时改为整体替换而不是平均，否则闪烁的光标会变成鬼影。

### 收尾顺序（不可省步）

`finish()` 必须按序：置 `sampling=false` → `notify_all` 唤醒被背压挡住的采集线程 → 关高亮与面板 → `join` 采集线程（≤250 ms，让在途的 grim 落地）→ drain 队列 → 处理 `latest_frame` → `result()`。

省掉 drain 或 `latest_frame` 会静默丢掉最后一屏内容。`latest_frame` 独立于队列保存，就是为了关掉「grim 已抓到最终滚动位置但 GTK 还没处理完」这个竞态。

### 采集用线程，不用 GLib timer

单次 grim 阻塞 16–22 ms。要达到足够的采样率就得用 ≤50 ms 的 timer，那会卡死 GTK 主循环。worker 线程 back-to-back 抓帧、`Arc<Mutex<VecDeque>>` + `Condvar` 背压（**队满时暂停采集而不是丢帧**，丢一个桥接帧就足以逼用户回滚），主线程用 `glib::idle_add` 消费。

### 不做自动滚动

Wayland 下普通应用无法安全合成滚轮事件。这是事实陈述，不是待办项。

## 5. UI 侧的硬约束

这些是原实现用户反馈换来的，改掉等于回归。

- **UI 绝不进入产出图**：选区边框是四个独立的 layer-shell 窗口且只画在选区外；长截图面板锚在选区未覆盖的一侧（挑最宽的空隙，都不够时退到底部）；overlay 关闭后等 250/300 ms 才开始抓帧。
- **长生命周期窗口必须 `NON_UNIQUE`**：GTK 默认单实例下，第二次截图只向第一个进程转发 `activate`，后者重新显示**旧内容**然后退出并删掉自己的 `--cleanup` 临时文件，新截图永久丢失。overlay、pin、result 三个都设了。
- **孤儿 overlay 逃生阀**：overlay 持有全屏 EXCLUSIVE 键盘抓取，一旦映射到用户看不见的地方，Escape 永远不来，进程僵在 `Application::run`。看门狗认**真实输入时间戳**（45 s 静默取消，5 s 轮询），不认 GTK 焦点事件——layer-shell 下焦点 enter/leave 不可靠，早期纯焦点方案会把「只用鼠标标注」误判为用户离开。焦点丢失只启动 10 s 宽限计时，期间任何输入都撤销它。
- **标注模式豁免看门狗**：边看屏幕边想批注是合理的静止。
- **单一幂等出口**：`emit` 带一次性闩锁，否则迟到的点击与焦点取消会竞态，导致裁剪或取消执行两次。
- **标注 stroke 光栅化进缓存 surface**：否则每帧重放上百个画笔点会越来越慢。

## 6. 控制服务协议

JSON over Unix socket，换行分帧，单条上限 64 KiB。命令 `ping`/`status`/`action`/`shutdown`。

响应是**单一形状**：`ok` 表示守护进程理解了请求，`accepted` 表示真的启动/切换了动作。「理解但拒绝」是 `ok:true, accepted:false`，客户端借此区分 busy 与「守护进程不在」。

`action` 是闭集枚举（`region`/`long`/`pin-last`），未知动作在反序列化阶段就被拒，早于任何进程 spawn。

**`long` 是开关式**：已有长截图在跑时不返回 busy，而是向活动进程发 `SIGUSR1` 让它完成，返回 `{accepted:true, toggled:true}`。这样用户不必把指针移回浮动面板——面板本来就得留在采样区外。`region`/`long` 互斥，`pin-last` 不互斥（它只是重钉剪贴板内容）。

安全边界：`flock` 独占 `service.lock`；runtime dir 0700；socket 0600（socket 能启动进程，不许他人访问）；子进程 stdout/stderr 追加进 `service.log`（GUI 动作没有终端，否则 panic backtrace 不可见）；子进程设 `VELLUM_BYPASS_SERVICE=1`（否则它会把请求再打回守护进程形成无限循环）并 `setsid()`（动作窗口要活过守护进程重启）。

accept 循环用 `poll()` 阻塞等待而不是 sleep 轮询。**这曾是一个真实缺陷**：25 ms 的 sleep 让每次快捷键都固定多付 25 ms（`PERFORMANCE.md` §3）。`REAP_INTERVAL` 现在只是「多晚注意到子进程已结束」的上限，不是请求延迟。

## 7. 与 Python 版的行为差异

`ARCHITECTURE.md` §8.4 要求逐条说明。除算法层面的逐位等价（`PERFORMANCE.md` §5）之外，有意的差异如下。

| 差异 | 理由 |
| --- | --- |
| 截屏用 `grim -t ppm` 而非 `-t png` | 每帧省约 99 ms，PNG 编码在这里是纯浪费 |
| accept 循环用 `poll()` 而非 25 ms sleep | 修掉每次快捷键固定 25 ms 的延迟 |
| `route_action` 直接发 action，失败才 `ensure_service` | Python 每次动作前先发一次独立 ping，白付一个往返 |
| 配置主路径为 `~/.config/vellum/config.toml` | 与仍在服务的 Python 版隔离；旧路径作只读回退 |
| 窗口规则按 app-id / class 匹配，不按 title | Rust 版窗口标题是本地化文案（「vellum 钉图」），改语言就失效；app-id 稳定 |
| 同时支持 niri 与 Hyprland，不再是 niri-only | 用户要求「这个版本适配 niri 和 hyprland」；抽象层见 §8 |
| `install.sh` 不 clone，直接构建脚本所在的树 | 没有远端可漂移，没有第二份源码要同步 |
| `install.sh` 的依赖复核直接跑 `vellum doctor` | Python 版有两份会漂移的依赖清单；doctor 是唯一事实来源 |
| `doctor` 检查 GTK4/layer-shell/leptonica 运行库，不检查 Python 模块 | 检查本构建真正加载的东西 |
| 托盘用 ksni（Rust + zbus）而非 GTK 3 + Ayatana | 托盘进程不再链接任何 GTK |
| 未移植 `run_result` 与 `Annotator::cycle_color/cycle_width` | Python 里已是死代码（全仓库无调用者） |
| `anno.done` 无内容时返回选区工具栏而不是直接完成 | 修正：Python `_exit_annotate(apply=True)` 本就是这个语义，早期移植写错了 |
| 钉图窗口缩放先查合成器实时尺寸，再回退自记账 | 用户用合成器键位改过窗口大小后，自记账会漂移 |

## 8. 合成器抽象（niri + Hyprland）

`ARCHITECTURE.md` §22 原本写的是 niri-only。用户后来要求「这个版本适配 niri 和 hyprland」，所以加了一层抽象。

### vellum 到底需要合成器做什么

只有四件事，而且**只针对自己的长生命周期窗口**（钉图窗、OCR/翻译结果窗）：

1. 按 pid 找到自己刚映射的窗口
2. 把它移到浮动层
3. 读它当前的真实尺寸
4. 精确设置它的尺寸

选区 overlay、长截图面板、选区高亮都是 layer-shell surface，**不需要任何合成器专属代码**——`wlr-layer-shell` 在两个合成器上行为一致。所以抽象面很窄，实现放在 `vellum-core/src/compositor/`（不是 UI 二进制里），因为 `doctor` 也要报告检测结果，一份实现不会漂移，两份会。

### 两条铁律

**缺失不是错误。** 未识别的合成器让四个入口全部返回 `None`/`false`，vellum 退化成普通 Wayland 客户端：截图、标注、OCR、翻译、托盘全部照常，只是钉图窗不会自动浮动、缩放走 GTK 而非合成器。这是受支持的降级模式，`doctor` 报 Warning 而非 Error。

**按环境变量探测，不主动连接。** `NIRI_SOCKET` 存在 → niri；`HYPRLAND_INSTANCE_SIGNATURE` 存在 → Hyprland。主动连接探测会给快捷键热路径加延迟，而且在「两个二进制都装了但只跑一个」时会误判——本机正是如此（`/usr/bin/niri` 存在但跑的是 Hyprland）。

窗口句柄用 `enum Window { Niri(u64), Hyprland(String) }`：niri 用数字 id，Hyprland 用十六进制 address 字符串，两者不可互换，用类型区分防止误传。

### 为什么按 pid 查窗口，而不是操作「聚焦窗口」

映射与延迟 60 ms 的浮动调用之间，用户完全可能已经切换焦点。浮动别人的窗口是可见且困惑的副作用。只有拿不到 handle 时才退回 `float_focused()`。

### Hyprland 协议的三个坑

都是实测踩出来的，写在这里免得再花一遍时间：

- **响应末尾没有换行**。`j/clients` 返回合法 JSON 数组但不带 `\n`，用 `read_line` 会挂住，必须 `read_to_end`。niri 相反，它以换行分帧。
- **请求是纯文本命令，不是 JSON**。socket 路径 `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock`。
- **这个 build 的 `dispatch` 参数按 Lua 解析**。经典文本语法 `dispatch setfloating address:0x...` 必然失败并报 Lua 语法错。正确形式是 `hl.dsp.window.float({ action = "enable", window = "address:0x..." })`。用 `"enable"` 而非 `"toggle"` 保证幂等。精确改尺寸是 `hl.dsp.window.resize({ x = w, y = h, window = ... })`，`relative` 默认 false，一次调用设两维。

API 的权威来源是 `/usr/share/hypr/stubs/hl.meta.lua`：`hyprctl eval` 只回 `ok` 拿不到返回值，无法自省。

### 快捷键：两个合成器能做的事不一样

这是本次最重要的判断。键位都在用户拥有的文本文件里，但 vellum 能安全做的事不同：

- **niri（KDL）**：真的写。插入带标记的托管块 → `niri validate` → 失败恢复备份 → 运行中尝试 reload。任一按键冲突就整组不写。
- **Hyprland 经典 `hyprland.conf`（hyprlang）**：也能写。同样的备份 → `hyprctl reload` → `hyprctl configerrors` → 失败回滚。
- **Hyprland Lua 配置**：**永不写**，只打印可粘贴的片段。Lua 配置是代码不是设置，改它等于往用户程序里拼代码；更关键的是这个 build 直接拒绝 `hyprctl keyword`（原文 `keyword can't work with non-legacy parsers. Use eval.`），连生效前校验的手段都没有。没有校验就不该自动改。

discovery（`shortcuts list`）两边都支持且只读，但**必须解析配置文本，不能问合成器**：`hyprctl binds` 把每条 Lua 绑定都报成 `dispatcher: __lua` 加一个不透明数字，活着的合成器说不出哪个键启动了 vellum；而且 `hyprctl -j binds` 在此 build 输出非法 JSON（`"keycode": Comma` 未加引号）。
