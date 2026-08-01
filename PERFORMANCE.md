# PERFORMANCE.md

ARCHITECTURE.md §6 要求性能结论必须来自实测，不能凭「Rust 应该更快」。本文档记录测量方法、可复现命令和本机实测数据，并列出与 Python 版的行为差异（§8.4）。

历史主表与本轮补测都在同一台机器上采集；涉及合成器的条目在各节明确会话，不能互相替代：

- AMD Ryzen 7 7735H（16 线程），Arch Linux
- rustc 1.97.0，release profile（`opt-level=3`、`lto="fat"`、`codegen-units=1`）
- Python 3.14.6 + numpy 2.5.1 + Pillow 12.3.0（对照基线）
- 历史 overlay/窗口主表：Hyprland 0.56.0；2026-08-01 的 screencopy 与最终 smoke：niri

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
| 不透明 | vellum | **151.5 ms** | 105.3 ms | 1.04 ms | 46.2 ms |
| 不透明 | Python | 1419.9 ms | 831.2 ms | 8.23 ms | 588.7 ms |
| 半透明 | vellum | **135.1 ms** | 93.6 ms | 0.93 ms | 41.5 ms |
| 半透明 | Python | 1317.9 ms | 749.9 ms | 7.42 ms | 568.0 ms |

两边都是 101 帧全部使用、输出 900x2700、keyframe 2.2 MiB、走了 offline rebuild，即同等工作量下 **约 10 倍**。ARCHITECTURE.md §4.2 记载的 Python 基线 1.4~2.3 秒与此处复现的 1.42/1.32 秒一致。

keyframe 内存 2.2 MiB 远低于 48 MiB 硬上限，`_trim_keyframes` 在该规格下不会触发。

### 超长画布失配恢复

完整画布重定位不能对每个候选位置再比较一个完整 viewport，否则一次 miss 是 O(canvas height × viewport height)，而且 robust 路径会在每个位置创建并分区行分数。用 release 临时压力探针预先构造 **50,000 行 canvas、700 行 viewport**，再送入一帧确定不匹配的画面；构造阶段不计入 miss：

| 实现 | 单次完整 miss |
|---|---:|
| 逐位置完整普通 + robust 扫描 | 167 ms |
| 双门粗排 + 有界精确候选 + viewport RGB window | **38–42 ms** |

当前实现对全画布只做 16 行 × 6 稀疏列的 O(canvas height) 粗排，行签名与 RGB 分别保留最多 512 个候选，再用原始完整 scorer 和不变的 9.0 / 32 / 24 门限决定是否重定位。粗排不参与接受判定；精确、局部损坏 robust、快速前进三个相邻回归均通过。压力文件属于一次性 probe，测完已删除，仓库只保留确定性功能回归。

### 周期列表回滚

新增的 `repeating_list_rollback` 回归使用 72 px 周期卡片、240 px viewport 与 20 px 步长，执行“向下 600 px → 回滚到 100 px → 再向下超过旧边界”。旧热路径在第一帧回滚时把 -20 px 选成结构同相位的 +52 px，在线画布由应有的 1040 px 膨胀到 2840 px；禁用离线重建与默认重建两条路径都确定性失败。修复后回访阶段每帧 `last_added == 0`，第一帧方向即为 -20 px，在线输出和离线重建输出都与唯一页面跨度逐字节一致；相邻的三帧边界 `0 → 20 → 0` 也验证了历史只有 seed 与 newest 时仍能精确回访；`600 → 500` 的速度突变用例则锁定“完整画布候选不得用较差分数覆盖历史中的 0/0 精确命中”。该修复只在首个候选的 sparse-RGB 重叠并非完全一致时继续查看至多 6 帧历史，普通静态滚动仍在第一帧短路，接受阈值没有变化。

### “短图”分层遥测方法

`VELLUM_LONGSHOT_TRACE=1 vellum long` 会把一次 session 的 daemon/UI 生命周期和逐帧数字写成带随机 id 的 JSON-lines。它不写像素或运行期文本，因而可以在真实网页上长期复现而不把页面内容带入日志。关键判读：

- `capture_successful` 与 `capture_exact_duplicates`：合成器实际交付了多少 distinct viewport；
- `queue_enqueued/dequeued/max_depth`：是否背压、是否有尾帧只落到 `latest`；
- 每个 `stitch_frame` 的 `decision/shift/added/diff/canvas_height/recovered`：是拒绝、已捕获回访、完整画布 re-anchor，还是确实增加唯一行；
- `finish_started`、`capture_worker_joined`、`finish_latest_frame`：完成时 queue/latest/in-flight 是否收干净；
- `session_summary` 的 online/output height 与 `offline_rebuilt`：在线 canvas 正确但离线重建变短，还是输入/在线阶段已经缺桥。

trace 未开启时 emitter 本身不分配或序列化；capture sequence/汇总计数仍使用已有 queue mutex，以便尾帧去重语义不依赖诊断开关。其对 101 帧 stitch benchmark 与 niri capture cadence 的影响必须在 Phase F 重跑后再记录，当前不把“clippy/tests 通过”冒充性能结论。

### recorder UI 隔离的 niri 像素证明

ignored live test `recorder::tests::live_solid_fixture_proves_recorder_ui_has_zero_sampled_pixels` 在当前单输出 niri（1920×1080 logical、scale 1）短暂映射一个已知 RGB `[31,93,167]` 的 full-output Overlay layer，随后只在内存中创建真实 `Recorder`、可见 Overlay panel 与四条 highlight，并抓取中央 600×500 rect；不读取原桌面内容、不写任何 PNG/frame。命令：

```sh
# full panel（600×500）
VELLUM_LONGSHOT_TRACE=1 cargo test --locked --release -p vellum-ui \
  recorder::tests::live_solid_fixture_proves_recorder_ui_has_zero_sampled_pixels \
  -- --ignored --exact --nocapture --test-threads=1

# compact panel（1280×500）
VELLUM_LONGSHOT_TRACE=1 VELLUM_TEST_LONGSHOT_COMPACT=1 \
  cargo test --locked --release -p vellum-ui \
  recorder::tests::live_solid_fixture_proves_recorder_ui_has_zero_sampled_pixels \
  -- --ignored --exact --nocapture --test-threads=1

# micro 竖条（1600×900，只剩左右窄边缘）
VELLUM_TEST_LONGSHOT_MICRO=1 \
  cargo test --locked --release -p vellum-ui \
  recorder::tests::live_solid_fixture_proves_recorder_ui_has_zero_sampled_pixels \
  -- --ignored --exact --nocapture --test-threads=1

# micro 横条（1920×900，只剩上下窄边缘）
VELLUM_TEST_LONGSHOT_MICRO_HORIZONTAL=1 \
  cargo test --locked --release -p vellum-ui \
  recorder::tests::live_solid_fixture_proves_recorder_ui_has_zero_sampled_pixels \
  -- --ignored --exact --nocapture --test-threads=1

# daemon-managed hidden panel（1800×1000，连 micro 也无安全位置）
VELLUM_LONGSHOT_TRACE=1 VELLUM_TEST_LONGSHOT_HIDDEN=1 \
  cargo test --locked --release -p vellum-ui \
  recorder::tests::live_solid_fixture_proves_recorder_ui_has_zero_sampled_pixels \
  -- --ignored --exact --nocapture --test-threads=1

# direct hidden（无 daemon finish endpoint，必须在采样前失败）
VELLUM_TEST_LONGSHOT_DIRECT_HIDDEN=1 \
  cargo test --locked --release -p vellum-ui \
  recorder::tests::live_solid_fixture_proves_recorder_ui_has_zero_sampled_pixels \
  -- --ignored --exact --nocapture --test-threads=1
```

fixture 使用 Overlay 而不是 Top：用户 shell 可能常驻透明 OSD/notification Overlay surface，Top fixture 无法隔离它们；fixture 先 map、Recorder surface 后 map 于同一层，所以既覆盖既有无关 surface，又不会掩盖之后创建的 vellum surface。

2026-08-01 最新 release 构建顺序实测六种真实分支：600×500 选区得到 `with_preview`，1280×500 得到 `without_preview`；1600×900 得到右侧 micro 竖条，1920×900 得到下方 micro 横条；1800×1000 才进入 daemon-managed `hidden/no_safe_space`，同尺寸 direct hidden 同步返回 `None + 1 warning`，并断言 `sampling=false`、capture worker 不存在。一次带 trace 的 release 测量得到 full **332×401**、compact **280×242**、micro 横条 **298×62**、micro 竖条 **94×176**；debug/字体分配的相邻运行曾为 full 332×404、compact 280×245、竖条 94×177，因此安全逻辑从不把单个硬编码尺寸当真值，而是逐次测量最大 natural size、固定 request，并在 map 后再验 actual allocation。前五个采样分支均为 `output_geometry_proven=true`，最终图尺寸与选区一致；按每通道容差 1 统计，偏离 fixture 的像素都为 **0**。full/compact/micro 强制 panel 可见且 micro 还断言选择阶段 envelope 不小于当前 measured footprint；managed/direct hidden 强制 panel 不可见，direct 分支证明没有留下无法完成的后台采集。六个分支每次结束后结构化查询的 vellum layer 数量都为 0，fixture/panel/highlight 均已收口。

可访问性定向 smoke 也沿用同一纯色/零像素 fixture：`GTK_THEME=Adwaita:hc` 下 micro 竖条 exit 0；`VELLUM_TEST_LONGSHOT_LARGE_TEXT=1` 在 USER CSS priority 注入 26px 字体（约为正文 13px 的 200% 压力条件），400×300 选区仍保留全部状态与主操作，测得 full 464×555、compact 406×301、micro 横条 484×74、竖条 166×251，map 后安全校验、零污染和 layer 清理均通过。确定性 fixture 截图的原尺寸视觉检查确认大字状态文案换行但不遮挡实时画面、累计高度或取消/完成按钮；这不替代真实桌面的 Orca 手工验收。

该证据覆盖当前 niri 单输出/scale 1。多输出或 capture/GDK scale/geometry 不一致时生产代码 fail closed：daemon 管理的 capture 不映射无法证明安全的 recorder UI，只写 trace；direct capture 则在采样前失败。桌面通知不会在采样前或采样中作为 fallback，因为它同样可能入镜。不能把单屏证据外推成未经验证的多屏安全结论。Hyprland 的相同纯色 live test 仍需切换会话后执行，不能用 niri 结果代替。

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
| ping | **0.046 ms** | 0.042 ms | 0.065 ms | 0.088 ms | 0.327 ms |
| status | **0.049 ms** | 0.042 ms | 0.072 ms | 0.108 ms | 1.015 ms |

落在 §6 要求的亚毫秒区间。

### 这个基准抓到的真实缺陷

第一次测量：

```
ping     mean  25.149 ms  p50  25.132 ms  p95  25.251 ms  p99  25.682 ms  max  26.281 ms
status   mean  25.156 ms  p50  25.131 ms  p95  25.246 ms  p99  25.677 ms  max  29.962 ms
```

**每个百分位都平齐在 25 ms**，正好等于 accept 循环里 `thread::sleep(25ms)` 的长度。根因：`daemon.rs` 用 `set_nonblocking(true)` + `WouldBlock` 分支 sleep 轮询，一个在 `WouldBlock` 检查刚过之后到达的连接必须等满整个 sleep。这不是测量噪声，是每次按快捷键都真实支付的延迟，而 §6 给整个 socket 跳跃的预算只有亚毫秒到个位数毫秒。

修复：`WouldBlock` 分支改为 `poll()` 阻塞等待（`wait_readable`），超时值 `REAP_INTERVAL = 25ms` 只决定「多晚注意到抓图子进程已结束」，有连接到来时 `poll()` 立刻醒。**25.149 → 0.046 ms，约 546 倍**。

回归验证：`cargo test -p vellum-ipc` 28 项全过，含 long 开关语义、互斥拒绝、子进程收尸、日志轮转、shutdown 路径。

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

长截图现已独立实现持久 `wlr-screencopy` 后端：一条 Wayland 连接、xdg-output 逻辑几何、复用 wl_shm buffer，事件等待同时 poll Wayland fd 与 stop eventfd。2026-08-01 在当前 niri 会话用 release 构建强制连续执行 31 次普通 copy、每次 900×700（像素只在内存中校验后丢弃），总计 62.44 ms，均值 **2.01 ms/帧**；同区域逐帧 `grim -t ppm` 为 16 ms，原始 copy 约快 8 倍。实际采样在首帧后用 `copy_with_damage`，只在内容变化时交付帧，并每 1 秒普通 copy 一次作为静止画面 heartbeat，避免 2 ms 原语反过来制造数百张重复帧。协议不可用、跨输出或运行时错误时回退 grim；fallback 通过 memfd 接收 PPM、每次最多等待 2 秒并整组杀死超时子进程，因此稳定性不依赖 screencopy 一定可用。

这里优化的重点不是只省 14 ms，而是消除“每帧新建一个无 timeout 子进程”的无界状态：持久连接可被完成信号立即唤醒，连续失败有 UI 状态，输出变化最多触发一次降级。当前 niri 已用 3 帧 2×2 与 31 帧 900×700 两种真机路径验证 buffer 复用；Hyprland 仍需在切换到该会话后做同一真机 smoke，不能用 niri 的通过替代。

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

**初版移植结论：质量等价，CJK 残缺当时是 Tesseract 与单候选预处理管线的共同限制。** 手写的形态学 CLOSE / `divide` / Otsu 与 OpenCV 在这批图上给出一致结果；下面的多场景增强复测是在这一基线上继续改进，而不是改写原测量。

#### 多场景增强复测（2026-08-01）

在原有 fixture 之外增加 8 张确定性 PIL 合成图，字体仍为 `SourceHanSansCN-Regular.otf`，真值统一为两行 `Vellum OCR 2026` / `暗淡彩色文字识别`。准确率先移除空白与标点，再按字符 Levenshtein 距离计算；“旧”是公开 `v0.1.0` 提交，“新”是本节实现，均为 release 构建并调用同一套 Tesseract 5.5.3 `chi_sim+eng`。

| fixture | 场景 | 旧准确率 | 新准确率 | 旧耗时 | 新耗时 |
|---|---|---:|---:|---:|---:|
| clean | 普通浅色面板 | 100.0% | 100.0% | 591 ms | 555 ms |
| dim-light | 浅色底上的暗淡小字 | 95.2% | **100.0%** | 646 ms | 1797 ms |
| dim-dark | 暗色底上的暗淡字 | 100.0% | 100.0% | 554 ms | 1660 ms |
| isoluminant | 前景/背景灰度完全相同、仅色相不同 | 4.8% | **100.0%** | 1031 ms | 3539 ms |
| color-interference | 彩色纹理与斜线干扰 | 48.8% | **95.2%** | 698 ms | 2452 ms |
| faded-gradient-dark | 渐变底上的半透明暗字 | 100.0% | 100.0% | 576 ms | 573 ms |
| faded-gradient-light | 渐变底上的半透明亮字 | 4.8% | **100.0%** | 598 ms | 1664 ms |
| faded-colored-dark | 暗色底上的低对比彩色字 | 100.0% | 100.0% | 559 ms | 1585 ms |

普通 clean 与旧路径耗时相同；额外成本只在场景分析要求 CLAHE、相反极性或颜色投影时支付。所有 Tesseract 尝试共用 30 秒总 deadline，不会按候选数线性放大最坏等待。历史并排表中的彩色强干扰 fixture 仍有 1 个 CJK 字错误，但已从包含大量噪声的 48.8% 提升到无额外噪声的 95.2%；仓库内新的可重复门禁使用独立合成样本，当前复跑为 10/10、100%。

#### 困难场景延迟收敛（当前修复）

在同一台机器、同一 probe 和同一 10 个固定 fixture 上，调整只涉及候选调度：等亮异色时把颜色投影提前，彩色繁忙背景先颜色后反极性，PSM 11 稀疏噪声过多时不再启动会放大噪声的 block retry（PSM 6 → 11 的救援仍保留），LocalContrast 的两种语言顺序并行后仍执行原来的逐行融合。识别门禁保持 **10/10、全部 100%**；耗时对比如下（单次墙钟，保留进程抖动）：

| fixture | 调整前 | 当前 |
|---|---:|---:|
| clean | 593 ms | 635 ms |
| dim-light | 1752 ms | 1310 ms |
| dim-dark | 1579 ms | 1265 ms |
| isoluminant | 4139 ms | **1119 ms** |
| color-interference | 4905 ms | **2496 ms** |
| faded-gradient-light | 1802 ms | 1904 ms |
| faded-colored-dark | 1650 ms | 1187 ms |
| banner-dim-color | 1485 ms | 1065 ms |

因此用户观察到的旧版慢路径确实是“困难场景用更多 Tesseract 候选换识别率”，但不是必须串行付完所有候选。现在保留同样的质量复核，最慢合成场景从约 4.9 秒降到约 2.5 秒。`VELLUM_OCR_TRACE=1` 可记录候选种类、PSM、预处理/Tesseract 耗时、置信度与最终选择，不记录 OCR 文本。

#### 可重复的本地回归门禁

历史表保留的是 v0.1.0 与增强提交的原始并排测量；为避免 fixture 只存在于临时目录，仓库另提供 `tools/ocr-regression.py`。它用固定随机种子生成 8 个双行场景和 2 个短横幅，在临时目录构建并调用 `vellum-text` 的 developer-only `ocr_probe` example，按同一 Levenshtein 规则返回人类表格或 `--json`，任何场景低于阈值即退出 1。脚本不读取截图目录或剪贴板。

```sh
# 依赖：python-pillow、Source Han Sans/Noto CJK、tesseract chi_sim+eng
python3 tools/ocr-regression.py
python3 tools/ocr-regression.py --json
```

该检查依赖本机字体与 Tesseract 数据，不冒充纯 Rust CI；缺依赖或 fixture/probe I/O 失败会明确退出 2（unavailable），与完成评分但不达标的退出 1 分开。Rust 侧的候选选择、CLAHE/PCA、TSV 解析和多栏融合仍由无外部依赖的单元测试覆盖。2026-08-01 在 Tesseract 5.5.3 + Source Han Sans CN 上从干净临时目录复跑，10/10 场景均为 100%；最终调度下 warm run 的 clean 约 635 ms，最慢的强彩色干扰约 2496 ms。

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

1. **普通抓屏用 `grim -t ppm` + 手写 P6 解码，长截图优先持久 `wlr-screencopy`**（Python 每帧 `grim -t png`）。screencopy 失败时才回到有界 grim，见 §3。
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

核心合成器集成已分别在 Hyprland 0.56.0 与 niri 真机验收。以下区分既有窗口/快捷键路径与本轮新增的长截图采集路径，不能用一个合成器的结果替代另一个。

**Hyprland 既有路径：已真机验收。** 用 vellum 自己的 pin 窗口跑通了完整代码路径（不是裸 IPC 探测）：

| 检查 | 结果 |
| --- | --- |
| `compositor::detect()` | `Hyprland` |
| `window_for_pid(pid)` | `Hyprland("0x5620614d6660")`，按自己的 pid 找到窗口 |
| 自动浮动（map 后 60 ms） | 窗口以 `float=True` 出现，无需任何 window rule |
| `window_size` 回读 | `Some((640, 480))` |
| `set_window_size(760, 520)` | 返回 `true`，实测 `size=[760, 520]` |
| 用户其他窗口 | 5 个窗口全程 `float=False`，尺寸未变 |

layer-shell 行为（overlay 呈现、namespace、exclusive zone）也在 Hyprland 上验证通过，本文档的合成器侧延迟数据就是在这里采集的。

**niri 既有路径：已真机验收。** 合成器探测、按 pid 找自己的窗口、pin 浮动与精确改尺寸、layer-shell overlay，以及迁移后的快捷键均曾在 niri 会话跑通。

**本轮长截图采集改动的环境边界：** 2026-08-01 的最终验证会话是 niri（`NIRI_SOCKET` 已设置、`HYPRLAND_INSTANCE_SIGNATURE` 未设置）。忽略测试在内存中连续采集 3 帧，覆盖持久 `wlr-screencopy` 连接、damage copy 与 wl_shm buffer 复用；像素没有写盘。当前会话无法对新增后端做 Hyprland 真机 smoke，因此该项是“不可用”，不是通过。安装新构建后的完整 GUI 手动滚动手感也仍属于环境依赖验收。

**Hyprland 快捷键的已知限制**：Lua 配置下，`vellum shortcuts install` 只打印可粘贴片段、不自动写入（理由见 `DESIGN.md` §8：该 build 拒绝 `hyprctl keyword`，没有生效前校验手段）；经典 `hyprland.conf` 格式可以自动写入。两种格式的示例都在 `contrib/`，实际用户配置是否应用需由安装流程另行验证。
