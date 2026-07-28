# PERFORMANCE.md

ARCHITECTURE.md §6 要求性能结论必须来自实测，不能凭「Rust 应该更快」。本文档记录测量方法、可复现命令和本机实测数据，并列出与 Python 版的行为差异（§8.4）。

所有数据都在同一台机器、同一次会话内采集：

- AMD Ryzen 7 7735H（16 线程），Arch Linux
- rustc 1.97.0，release profile（`opt-level=3`、`lto="fat"`、`codegen-units=1`）
- Python 3.14.6 + numpy 2.5.1 + Pillow 12.3.0（对照基线）
- 合成器：**Hyprland 0.56.0**（见文末「环境偏差」）

Debug 构建的数字不要用来比较：形态学与拼接热路径在 debug 下比 release 慢一个量级，实测 OCR debug 比 Python 慢约 2 倍、release 反而更快。

---

## 1. 长截图拼接

### 方法

`crates/vellum-stitch/benches/longshot.rs`，规格照 ARCHITECTURE.md §4.2：**101 帧 900x700，每帧滚动 20 px**，输出 900x2700。

```sh
cargo bench -p vellum-stitch
```

刻意不用 criterion。用户等的是「松手到出图」的一次真实墙钟时间，而采样型 harness 只会报 per-`add` 吞吐，尾部延迟实际住在 offline rebuild 和最后一次 `vstack` 里。帧全部预生成，图像合成不进计时区；先跑一次未计时 warm-up，避免第一轮付惰性缺页和冷 rayon pool 的账。

两组场景：不透明，以及 `apply_translucency(0.72, [24,26,38])` 的半透明窗口（Python 基线最慢的场景）。

### 数据

| 场景 | 实现 | 总计 | add | 每帧 | finish |
|---|---|---|---|---|---|
| 不透明 | vellum | **142.1 ms** | 90.2 ms | 0.89 ms | 51.9 ms |
| 不透明 | Python | 1419.9 ms | 831.2 ms | 8.23 ms | 588.7 ms |
| 半透明 | vellum | **140.4 ms** | 93.4 ms | 0.92 ms | 47.1 ms |
| 半透明 | Python | 1317.9 ms | 749.9 ms | 7.42 ms | 568.0 ms |

两边都是 101 帧全部使用、输出 900x2700、keyframe 2.2 MiB、走了 offline rebuild，即同等工作量下 **约 10 倍**。ARCHITECTURE.md §4.2 记载的 Python 基线 1.4~2.3 秒与此处复现的 1.42/1.32 秒一致。

keyframe 内存 2.2 MiB 远低于 48 MiB 硬上限，`_trim_keyframes` 在该规格下不会触发。

### 输出逐位等价

只比性能不足以证明移植正确。方法：让 Rust 把**它实际喂进 stitcher 的那 101 帧**原样 dump 成 PNG，再把同一批 PNG 喂给 Python `Stitcher(9.0, 4, preview=False)`，逐像素比对两边的输出。用同一批帧而不是两边各自生成，才能排除 fixture 分歧。

结果：两个场景都是

```
identical pixels 2430000/2430000 (100.0000%)  max |delta|=0  mean |delta|=0.00000
```

即 vellum 不只是行为等价，而是逐字节复现 Python 输出。阈值、行签名匹配、稀疏 RGB 校验、融合权重和固定区域处理全部对齐。

该比对是一次性验证，脚本与 dump 已删除，不留在仓库里。

---

## 2. 控制服务 socket 往返

### 方法

`crates/vellum-ipc/benches/roundtrip.rs`：

```sh
cargo bench -p vellum-ipc
```

驱动**真实的 `daemon::run()` accept 循环 + 真实 Unix socket + 真实 client**，只把 runtime dir 换成私有临时目录。手写一个简化 `accept()` 会测到永远不会发布的代码。测 `ping`（快捷键真正等待的请求）和 `status`（CLI 与托盘每 2 秒调用，且会先收尸子进程，是两个只读命令里较慢的）。50 次 warm-up + 1000 次采样。

报告百分位而非只报均值：均值会掩盖「多数请求瞬间返回、偶尔卡满一个轮询周期」这个失败模式——而那正是本次抓到的缺陷。

### 数据

| 命令 | mean | p50 | p95 | p99 | max |
|---|---|---|---|---|---|
| ping | **0.046 ms** | 0.043 ms | 0.066 ms | 0.082 ms | 0.263 ms |
| status | **0.045 ms** | 0.042 ms | 0.065 ms | 0.088 ms | 0.206 ms |

落在 §6 要求的亚毫秒区间。

### 这个基准抓到的真实缺陷

第一次测量：

```
ping     mean  25.149 ms  p50  25.132 ms  p95  25.251 ms  p99  25.682 ms  max  26.281 ms
status   mean  25.156 ms  p50  25.131 ms  p95  25.246 ms  p99  25.677 ms  max  29.962 ms
```

**每个百分位都平齐在 25 ms**，正好等于 accept 循环里 `thread::sleep(25ms)` 的长度。根因：`daemon.rs` 用 `set_nonblocking(true)` + `WouldBlock` 分支 sleep 轮询，一个在 `WouldBlock` 检查刚过之后到达的连接必须等满整个 sleep。这不是测量噪声，是每次按快捷键都真实支付的延迟，而 §6 给整个 socket 跳跃的预算只有亚毫秒到个位数毫秒。

修复：`WouldBlock` 分支改为 `poll()` 阻塞等待（`wait_readable`），超时值 `REAP_INTERVAL = 25ms` 只决定「多晚注意到抓图子进程已结束」，有连接到来时 `poll()` 立刻醒。**25.149 → 0.046 ms，约 546 倍**。

回归验证：`cargo test -p vellum-ipc` 22 项全过，含 long 开关语义、互斥拒绝、子进程收尸、日志轮转、shutdown 路径。

如果只依赖「Rust 比 Python 快」的直觉，这个缺陷会一路带到用户手上，且症状（快捷键有一点点迟滞）几乎不会被归因到 socket 层。

---

## 3. 快捷键端到端延迟

§6 要求毫秒级测量方法，并且要用实测数据决定 overlay 是否值得常驻，不预设结论。

### 方法

两个互补测量，因为单独任一个都有盲区：

**进程内打点**（`crates/vellum-ui/src/trace.rs`）：`VELLUM_TRACE` 存在时才启用，未设置时只付一次 `env::var_os`。每个 mark 同时记录相对进程启动的偏移和墙钟时间戳，因此外部 harness 记下自己 spawn 前的时间就能算出**含进程创建**的真实端到端值。打点位置：`process-start`、`capture-start`、`capture-done`、`overlay-first-draw`（第一次 draw 才是用户真正看见 overlay 的时刻，后续重绘不计时）。

**合成器侧观测**：轮询 `hyprctl layers` 等 `namespace: vellum-overlay` 出现。这个方法对 Rust 和 Python 同样适用，所以两边可比。轮询本身的分辨率实测约 6 ms，读数时要带上这个误差。

### 数据（Rust）

进程内分解，6 次运行：

| 阶段 | 偏移 |
|---|---|
| `process-start` | +0.01 ms |
| `capture-start` | +0.04 ms |
| `capture-done` | +30~37 ms |
| `overlay-first-draw` | +126~158 ms |

含进程创建的总计：直接 spawn `vellum-ui` **158~203 ms**；经 `vellum region` 走 execv 交接 **149~196 ms**。合成器侧观测 **151~188 ms**，与进程内打点一致。

execv 交接没有可测量的成本——`vellum` 路径不比直接 spawn `vellum-ui` 慢，这验证了「热路径二进制不链 GTK、用 execv 交接」的设计没有引入额外延迟。

### 数据（Python 对照）

同一合成器侧方法，`PNGSHOT_BYPASS_SERVICE=1` 绕过正在服务用户的 Python daemon：**443~558 ms**。

vellum 约 **2.9 倍** 快（155 ms vs 460 ms 中位）。

### overlay 是否值得常驻：不值得

预算分解给出了答案，而不是靠猜：

- `grim` 抓全屏 ≈ 32 ms
- GTK 初始化 + layer-shell + 第一次 paint ≈ 105 ms
- 其余（进程创建、参数解析、socket 往返）< 1 ms

常驻 overlay 只能省掉后者中的 GTK 初始化那部分，**抓屏那 32 ms 无论如何都要付**（背景必须是按下快捷键那一刻的画面，不能预先抓）。而常驻要付的代价是：一个始终活着的 GTK 进程、一个必须永不卡死的全屏 layer-shell surface，以及「陈旧 overlay 被重新呈现」这一整类故障——这正是当前设计用 `NON_UNIQUE` 明确避开的问题。155 ms 已经在「按下就出现」的感知区间内，用一整类新的故障模式换约 100 ms 不划算。

结论：**保持每次新建进程，不常驻 overlay**。

### 抓屏原语（§4.1）

| 命令 | 平均 |
|---|---|
| `grim -t ppm` 全屏 | **22 ms** |
| `grim -t png` 全屏 | 121 ms |
| `grim -t ppm` 900x700 区域 | 16 ms |

vellum 用 `-t ppm` 加手写 P6 解码，相比 Python 版的 `-t png`**每帧省约 99 ms**，且省掉一次 PNG 编码再解码的往返。这是长截图 `add` 之外最大的单项收益，也是采样率能提上来的前提。

关于直接走 `wlr-screencopy`：`grim -t ppm` 的 22 ms 里已经包含进程创建、协议往返和一次全屏拷贝。自己实现 screencopy 能省掉进程创建（约 1~2 ms）和 PPM 序列化，但要接管 dmabuf/shm 缓冲、多输出几何、格式转换和合成器差异。**当前不做**：长截图的瓶颈已经不在抓帧（0.89 ms/帧的拼接 vs 16 ms 的区域抓帧，抓帧是上限但 20 px/帧的滚动速度下 16 ms 已足够），端到端延迟的大头在 GTK 初始化而非抓屏。如果将来要提高长截图采样率上限，这是第一个该动的地方。

---

## 4. OCR 与翻译

### OCR

方法：用 PIL 生成 4 张 fixture（干净 / 暗色主题 / 单行横幅 / 噪声背景+半透明面板），同一批图分别喂给 vellum 与 Python 版。CJK 必须用 `SourceHanSansCN-Regular.otf`——第一版 fixture 用 Noto Sans 画中文得到的是豆腐块，OCR 读出乱码是对空框的忠实识别，属 fixture 缺陷。

| fixture | vellum（release） | Python |
|---|---|---|
| clean | 653 ms | 1110 ms |
| dark | 543 ms | 540 ms |
| busy | 700 ms | 730 ms |

识别质量：`dark`（走 invert 分支）与 `banner`（ratio≥2.4 且 h≤96 → psm 7）两边都完全正确；`clean` 的 Latin 行两边都完全正确，CJK 行两边都差一个字（`长截图拼接君成` vs `长截图拼接叶成`，ground truth `长截图 拼接 完成`）；`busy` 的 Latin 行两边都正确，CJK 行两边都严重乱码。

**结论：质量等价，CJK 残缺是 tesseract 与预处理管线的限制，不是移植缺陷。** 手写的形态学 CLOSE / `divide` / Otsu 与 OpenCV 在这批图上给出一致结果。

### 翻译

| 路径 | 耗时 |
|---|---|
`opencode serve` HTTP（复用常驻服务） | **2.96 s**
`opencode run` CLI 回退 | 7.03 s

服务路径约快 2.4 倍，这是 §2.7 优先复用常驻服务的实测依据。

验证服务路径真的在跑，用的是决定性方法而非看日志：**把 `opencode` 从 `PATH` 移除**，只剩 HTTP 路由可能成功——仍然返回正确译文。

两个容易写错的点已核实：服务器返回的 `parts` 数组包含 `step-start` / `reasoning` / `text` / `step-finish`，其中 **`reasoning` 项也带 `text` 字段**，按字段而非按 `type == "text"` 过滤会把模型的思考过程当成译文；一次性 session 在 finally 里 DELETE，跑完检查服务器上标题为 `vellum translation` 的 session 数为 0，不污染用户的 session 列表。

---

## 5. 与 Python 版的行为差异（§8.4）

功能等价，以下是刻意的差异及理由。

**性能相关**

1. **抓屏用 `grim -t ppm` + 手写 P6 解码**（Python 用 `-t png`）。每帧省约 99 ms，见 §3。
2. **`route_action` 直接发 action**，失败才 `ensure_service` 重试一次。Python 每次动作前先发一次独立 `ping`，白付一次往返。
3. **daemon accept 循环用 `poll()`**。Python 的 `socketserver` 是阻塞 accept，本来没有这个问题；是 Rust 版早期的 nonblocking+sleep 写法引入的，已修复（§2）。
4. **`vellum`/`vellumctl` 不链接 GTK**，用 execv 交接给 `vellum-ui`。Python 的 `fastctl.py` 同样避免 import GTK，但回退时需要注入 `LD_PRELOAD=/usr/lib/libgtk4-layer-shell.so`；Rust 版在构建期链接 gtk4-layer-shell，不需要 preload。

**平台与集成**

5. **窗口规则按 app-id / class 匹配，不按标题**。Python 的规则是 `match title="pngshot-pin"`，而 vellum 的 pin 窗口标题是本地化的「vellum 钉图」，照搬会永远不匹配。app-id（`ai.vellum.pin`、`ai.vellum.result`）来自 GTK `application_id`，不随界面语言变化；Hyprland 的 `class` 就是同一个值。
6. **托盘是独立二进制 `vellum-tray`**（ksni + zbus），Python 是 GTK3 + AyatanaAppIndicator3。两者都导出传统 `com.canonical.dbusmenu`——这是硬约束，已用 `GetLayout(0,-1,[])` 返回完整 13 项菜单树验证，不是只看到图标就算过。`vellum tray` 子命令保留，execv 到 `vellum-tray`。
7. **`install.sh` 不 clone 任何东西**，直接构建脚本所在的树。没有远端可漂移、没有第二份源码要同步。
8. **依赖检查只有一份**：`install.sh` 最后直接跑 `vellum doctor`，不再像 Python 版那样维护第二份会漂移的清单。
9. **OCR 走 `tesseract` 子进程**，不用 `leptess`。本机 soname 是 `libleptonica.so` 而 `tesseract-sys` 期望 `liblept.so.5`，且 leptess 需要 bindgen/clang 构建依赖；OCR 已在后台线程且窗口先开占位，进程 spawn 不在感知路径上。
10. **`SIGUSR1` 用 flag + 50 ms 轮询**。glib 0.22 没有 `g_unix_signal_add` 绑定（已逐层核实 glib/glib-sys/gio 三处），handler 里只置一个 `AtomicBool`（唯一异步信号安全的操作），主循环轮询。50 ms 对「按快捷键结束长截图」不可感知。

**配置与命名**

11. **配置在 `~/.config/vellum/config.toml`**，不存在时**只读**回退到 `~/.config/pngshot/config.toml`。vellum 永不写入 pngshot 的路径。
12. **托盘偏好在 `~/.config/vellum/tray.json`**，runtime 在 `$XDG_RUNTIME_DIR/vellum`，unit 名 `vellum.service` / `vellum-tray.service`。与仍在服务用户的 Python 版完全隔离。
13. **输出文件名前缀 `vellum-`**，pin 前缀 `vellum-pin`，长截图前缀 `vellum-long`。

**未移植的死代码**

14. Python 的 `run_result`（无调用者）、`Annotator.cycle_color`/`cycle_width`（颜色与粗细走 popup，不走循环热键）、`_update_status` 里只 remove 从不 add 的 `pngshot-alert` CSS 类，都没有移植。

**修正的行为**

15. `anno.done` 在标注内容为空时**只返回选区工具栏，不完成截图**。这与 Python `_exit_annotate(apply=True)` 的 `has_content()` 判断一致；Rust 版初稿曾无条件 confirm，是移植缺陷，已修。
16. `zoom_window` 优先从 `compositor::window_size` 读当前尺寸，而不是从自己记账的值推算。用户用合成器键位改过窗口大小后，自记账会漂移。

17. **同时支持 niri 与 Hyprland**（Python 版是 niri-only）。合成器在运行期按环境变量识别，两边都不在时退化为普通 Wayland 客户端。见 `DESIGN.md` §8。

---

## 6. 验收覆盖与环境限制

**当前会话运行的是 Hyprland 0.56.0**（`niri` 二进制存在于 `/usr/bin/niri`、`~/.config/niri/` 配置齐全，但 `NIRI_SOCKET` 不存在，进程列表里是 `Hyprland`）。所以两个后端的验收程度不同。

**Hyprland：已真机验收。** 用 vellum 自己的 pin 窗口跑通了完整代码路径（不是裸 IPC 探测）：

| 检查 | 结果 |
| --- | --- |
| `compositor::detect()` | `Hyprland` |
| `window_for_pid(pid)` | `Hyprland("0x5620614d6660")`，按自己的 pid 找到窗口 |
| 自动浮动（map 后 60 ms） | 窗口以 `float=True` 出现，无需任何 window rule |
| `window_size` 回读 | `Some((640, 480))` |
| `set_window_size(760, 520)` | 返回 `true`，实测 `size=[760, 520]` |
| 用户其他窗口 | 5 个窗口全程 `float=False`，尺寸未变 |

layer-shell 行为（overlay 呈现、namespace、exclusive zone）也在 Hyprland 上验证通过，本文档的合成器侧延迟数据就是在这里采集的。

**niri：只有单元测试与静态校验。** 本机不是 niri 会话，以下路径需要你在 niri 里确认：

- niri IPC（`compositor/niri.rs` 的浮动 / 按 pid 找窗口 / 读写尺寸）。协议形状与 Python 版一致且未改动，但没有活的 niri 可以对话。
- `vellum shortcuts install` 写入 `~/.config/niri/dms/keybinds.kdl` 后的实际按键行为，以及 `niri validate` 之后的 reload。生成的 KDL 已用 `niri validate` 校验通过（`contrib/niri-vellum.kdl`）。
- niri 预设分栏宽度（1/3、1/2、2/3、全宽）下的窗口布局（`ARCHITECTURE.md` §5 要求）。

**Hyprland 快捷键的已知限制**：你的 Hyprland 配置是 Lua 格式，`vellum shortcuts install` 对它**只打印可粘贴片段、不自动写入**（理由见 `DESIGN.md` §8：这个 build 拒绝 `hyprctl keyword`，没有生效前校验的手段）。经典 `hyprland.conf` 格式可以自动写入。两种格式的示例都在 `contrib/`。我没有改动你的任何合成器配置。
