# pngshot 交接文档

面向未来维护者（也可能是几个月后的自己）。记录**为什么代码是现在这样**、**踩过哪些坑**、**改动时要小心什么**。泛泛的功能介绍见 `README.md`，这里只写排障和根因。

---

## 0. 新对话冷启动接手（出 bug 时先读这里）

**项目位置**：开发仓库 `~/Projects/pngshot`（GitHub: `git@github.com:tjz123psh/-Screenshot-Tool.git`，分支 `main`）。系统日常运行的是一键安装的独立克隆 `~/.local/share/pngshot`（见 §1「两套启动器」，别搞混）。

**技术栈**：Python 3.11+，GTK4 + gtk4-layer-shell，Wayland/niri，纯 pacman 依赖（无 pip 包，`dependencies=[]`）。运行时用到 grim、wl-clipboard、tesseract、python-opencv、python-numpy、python-pillow、python-gobject、python-cairo。

**接手一个新 bug 的标准流程**：
1. 先在 §2 的 bug 清单里找**有没有同类症状**——很多问题（窗口劫持、覆盖层卡死、涂鸦消失、重叠不足、OCR 崩）都已记录根因和陷阱，别重新踩。
2. 复现：直接跑 `PNGSHOT_ROOT=~/Projects/pngshot ~/Projects/pngshot/scripts/pngshot region`（或 `long`/`pin-last`）。**必须走启动器**，否则 LD_PRELOAD 缺失 layer-shell 会坏（见 §1）。
3. 定位：GUI/交互问题看 `overlay/surface.py`（逃生阀、输入）、`overlay/app.py`（进程/单实例）；长截图看 `longshot/`；OCR 看 `services/ocr.py`；niri 交互看 `util/niri.py`。
4. 验证：无 GUI 环境只能做 `python3 -m py_compile` + 导入检查；OCR/拼接这类纯算法可写字符准确率/合成图基准量化验证（§2 OCR 条目有先例）。真机行为改完必须实际截一次确认。
5. 改完提交：见 §5 安全/提交注意；改完源码要生效到日常命令需重跑 `install.sh` 或直接用开发启动器。

**当前状态基线**：README 中文、curl 一键安装（`install.sh`）、OCR 双引擎+自适应背景处理、长截图预热，均已提交并真机确认可用。最近提交 `f67201b`（OCR 杂乱背景自适应）。

---

## 1. 运行架构（一句话版）

- 用户命令先通过 `controller.py` 的私有 Unix socket 发给轻量 `pngshot.service`，服务立即确认并 spawn **一次性动作进程**：`pngshot region` 抓全屏 → 全屏 `wlr-layer-shell` 覆盖层选区 → 选完执行动作。服务不可用时 CLI 会自动拉起、重试，再失败才回退直接运行。
- 后台服务不导入 GTK，只负责单实例、状态、日志和失败通知；`tray.py` 使用 GLib-only Ayatana AppIndicator + `Gio.Menu`（不加载 GTK），右键菜单提供动作与两个简单偏好。socket/诊断调用都在线程中执行，结果用 `GLib.idle_add` 回主线程。
- 需要长期存活的窗口（钉图 / OCR / 翻译）由覆盖层进程 **spawn 独立子进程**（detached），临时图经临时 PNG 传递；子进程完整载入图片后立即用 `--cleanup` 删除临时文件，窗口后续只持有内存图像。
- 长截图（`long`）**不 spawn**，留在本进程内：覆盖层选完区 → `app.hold()` 保活 → 后台线程按帧采样屏幕矩形 → 主线程拼接。

### 关键启动约束：LD_PRELOAD
PyGObject 会在 gtk4-layer-shell 之前链接 libwayland，导致 layer surface 失效。**启动器必须预加载** `/usr/lib/libgtk4-layer-shell.so`。直接 `python3 -m pngshot` 会坏，一定要走启动器脚本。见 `scripts/pngshot`。

### 两套启动器（别搞混）
- `scripts/pngshot`：开发用，`PNGSHOT_ROOT` 默认指向 `~/Projects/pngshot`（本仓库）。
- `~/.local/bin/pngshot`：`install.sh` 一键安装生成的，指向独立克隆 `~/.local/share/pngshot`。
- `~/.local/bin/pngshotctl`：指向同一启动器，推荐给 Niri 快捷键、状态栏和诊断调用。
- **系统日常用的是后者**，改本仓库源码不会影响已安装的命令，需重跑 `install.sh` 或直接跑 `scripts/pngshot`。

---

## 2. 已修 Bug 与增强档案（根因 + 位置 + 陷阱）

以下都是真实踩过的坑，按发现顺序累积。改到相关代码时先读这里，别重蹈覆辙。**新修的 bug 请追加到本节末尾**，保留根因和陷阱，方便下一个接手的人。

### Bug 1 — 长截图控制面板白边
- **现象**：面板圆角卡片四周有一圈白色。
- **根因**：面板窗口背景是 GTK 默认（浅色），圆角卡片只盖住内部，margin 和圆角外露出窗口底色。
- **修复**：`util/theme.py` 加 `.pngshot-transparent`（只清窗口节点背景，**不能用 `> *`**，否则会连卡片自身背景一起清掉）；`longshot/recorder.py` 给面板窗口加这个 class。`_CSS_VERSION` 要 +1 才会重载。
- **陷阱**：`theme.py` 的 CSS 是 `b"""..."""` **bytes 字面量，只能 ASCII**。别在里面写中文/破折号，会 `SyntaxError`。

### Bug 2 — OCR / 钉图窗口"单实例劫持"
- **现象**：不关旧 OCR 窗口就再截图选 OCR → 新图不显示、旧窗口被重新激活、行为诡异。
- **根因**：GTK `Gtk.Application` 默认单实例。同 `application_id` 的第二个进程只向第一个发 `activate` 后**立即退出**，于是新图的临时 PNG 被 `--cleanup` 删掉、旧窗口用旧图重新弹出。
- **修复**：给 detached 窗口的 Application 加 `Gio.ApplicationFlags.NON_UNIQUE`。位置：`util/result_win.py`（2 处：`run_result` + `run_text_action`）、`pin/window.py`（`run_pin`）、`overlay/app.py`（overlay 本身也有同样问题，见 Bug 3）。
- **陷阱**：新增任何会常驻的 detached 窗口进程，**记得加 NON_UNIQUE**，否则重现此 bug。

### Bug 3 — overlay 卡死 + 跨工作区跳转劫持
- **现象**：点截图后跳转到一个旧的、卡住的覆盖层画面；不同工作区都一样。
- **根因（两层）**：
  1. 覆盖层是全屏 layer-shell + EXCLUSIVE 键盘，**唯一退出路径**是用户在它上面触发动作。若在拖选中途切走工作区/焦点被抢，它收不到 Esc、又看不见，`app.run()` 永久卡在 poll → **僵尸进程**。
  2. 叠加 Bug 2 的单实例语义：新截图被转发 `activate` 到僵尸进程，compositor 把用户拽到僵尸所在工作区。
- **修复**：
  - `overlay/app.py`：overlay 的 Application 加 `NON_UNIQUE`（僵尸无法再劫持新启动）。
  - `overlay/surface.py`：加**三层逃生阀**保证覆盖层永远能自己退出——
    - `_emit`：唯一、幂等的结果出口（所有动作/取消/失焦/超时都走它，`_finished` 保证只触发一次）。
    - **失焦宽限**：拿到过焦点后失焦 → 启 10s 计时；重新聚焦取消它；超时才 cancel。
    - **空闲看门狗**：见 Bug 5（后来重写过）。
- **陷阱**：新增任何"结果出口"必须走 `_emit`，不要直接调 `on_result`，否则破坏幂等 + 逃生阀。

### Bug 4 —（并入 Bug 3 的三层逃生阀）
（早期把逃生阀拆成多条，已整合，无独立条目。）

### Bug 5 — 涂鸦时画面突然消失（逃生阀误杀）
- **现象**：标注涂鸦过程中，画到一半整个覆盖层连同涂鸦消失。
- **根因**：旧的空闲超时用 `not self._ever_focused` 判断"没在交互"，而 `_ever_focused` **只在 GTK 键盘焦点 enter 事件**里置 True。niri 下 layer-shell 的键盘焦点事件**不可靠/不触发**；用鼠标涂鸦走的是 `GestureClick`，从不碰键盘焦点 → 超时误判"从未交互" → 30s 后取消。
- **修复**（`overlay/surface.py`）：
  - 改用**真实输入活动时间戳** `_last_activity`，`_mark_activity()` 在**所有**输入处理器开头调用（`_on_pressed` / `_on_motion` / `_on_released` / `_on_key` / focus enter）。
  - **涂鸦模式完全豁免**所有自动取消（`if self.annotating: return`，在看门狗、失焦宽限、宽限到期三处都有）。
  - 超时改为**循环看门狗** `_on_idle_tick`：每 `_IDLE_POLL_S`(5s) 查一次，累计 `_IDLE_TIMEOUT_S`(45s) 完全无输入才取消。旧的一次性 30s focus-only 超时已删。
- **陷阱**：**不要依赖 GTK 焦点事件判断 niri 下 layer-shell 的用户在场状态**，它不可靠。要用输入活动时间戳。

### 长截图首用"重叠不足"（性能优化，非 bug）
- **现象**：第一次或头几次长截图报"重叠不足"，多用几次就正常。
- **根因（实测）**：grim 首次抓取 55.8ms、稳态 33.7ms（**首次慢 1.7 倍**）。worker 背靠背抓帧，第一帧慢就拉大帧1→帧2间隔，正常滚动速度下两帧位移越过重叠阈值。拼接计算本身只 3ms，非瓶颈。
- **修复**（`longshot/recorder.py` `_capture_loop`）：真实采集循环前先做**一次丢弃的预热抓取**，让交给拼接器的第一帧就是热的稳态延迟。

### OCR 识别偏弱（能力增强，非 bug）
- **现象**：屏幕截图 OCR 结果差，小字/低对比度尤其严重。
- **根因**：原来把原始截图直接喂 tesseract。屏幕约 96 DPI、字号小，而 tesseract 偏好 ~300 DPI。实测原图小字整行识别成乱码。
- **增强**（`services/ocr.py`，双引擎）：
  - **tesseract + 预处理（默认，本地/快/免网）**：`_preprocess` 配方 = 灰度 → 自动反色（深色主题）→ `autocontrast(cutoff=1)` → **LANCZOS** 放大 3x。这套配方和顺序是**字符级准确率基准跑出来的**（4 组测试图：常规/深色小字/超小字/低对比度），平均准确率从 ~0.85 提到 ~0.98，超小字 0.60→1.00。
  - **vision（可选）**：`engine = "vision"` 走 opencode 视觉模型，质量最好但慢(~2-3s)+需联网，失败自动回退 tesseract。默认**不用**（用户反馈模型慢且不稳定）。
- **杂乱背景（第二轮，真正的痛点）**：
  - **现象**：半透明终端/聊天窗口 OCR，橙色大标题崩成 `KEAR`、整段中文糊掉。
  - **根因（实测确认）**：崩溃**不是**颜色/粗体/大字号导致——同样文字纯色背景 score 0.71、杂乱背景（壁纸透过半透明窗口）暴跌到 **0.10**。真凶是**背景纹理**，文字坐在壁纸上，tesseract 无法把笔画从纹理里分出来。（前两轮"max-channel 修彩色""按字号调放大"的假设都被数据否决了。）
  - **修复**：`_preprocess` 改为**自适应双管线**。`_background_busyness()` 用大核形态学 CLOSE 估计背景层再取 std-dev：纯色 ~0、杂乱 ~6，阈值 3.0 有巨大安全边际。
    - 干净背景 → 原管线（不二值化）。
    - 杂乱背景 → `_prep_busy`：大核 CLOSE 估背景 → `cv2.divide` 除掉纹理 → 放大 → **Otsu 二值化**。把痛点场景从 0.22 救到 0.88+（合成图端到端到 1.0）。
  - **陷阱**：
    - 杂乱背景**必须先除背景再二值化**。直接 adaptive/Otsu 而不先 divide，会把纹理也变噪点，score 反而暴跌到 0.02。
    - bgsub 的核尺寸敏感：实测 **k=15 最稳健**，大核（21/25/31）在明亮壁纸上会崩到 0.05。别乱调。
    - 二值化只在**杂乱背景**用，干净小字二值化会掉分——所以才要 busyness 路由，别无脑全局二值化。
- **通用陷阱**：
  - 干净背景管线**别加锐化/二值化**，实测**降低**小字准确率（硬化抗锯齿伪影）。用 LANCZOS 不要用 BILINEAR。
  - 残余错误（`OCR→O0CR`、全角标点读成半角）是 tesseract **引擎层固有歧义**，预处理修不了；**别硬转标点**，会破坏英文行本该半角的标点，是负优化。
  - `recognize()` 现在收完整 `OcrConfig`；保留了对旧 `langs` 字符串的兼容（`_coerce_cfg`），改签名时别破坏它。
  - **教训**：第一轮合成基准全用纯色背景，虚高到 0.98 却漏掉真实场景。基准测试集必须覆盖真实痛点（杂乱背景/半透明窗口），否则数字骗人。

---

## 3. 长截图拼接器要点（`longshot/stitcher.py`）

- 用**行签名匹配**（每行压成 mean/contrast/edge 三数），**不是** `cv2.matchTemplate`。原因：模板匹配在真实文字/UI 上因亚像素渲染+抗锯齿得分只有 ~0.3，导致首帧后全被拒。
- canvas 用**块列表**存储（`_blocks`），append/prepend 是 O(1)，只在 `result()` 时 vstack 一次。别改回每帧 `np.vstack`（会退化成二次复杂度，越滚越卡→丢帧→"重叠不足"）。
- 预览缩略图也是增量的（每块只缩放一次并缓存）。别改成每帧全量重缩放。
- 支持**双向滚动**（`shift` 有符号，+下/-上），可从页面中间开始先往上再往下。
- 关键参数在 `config.py` 的 `LongshotConfig`：`poll_ms=0`（0=全速，别调大，否则帧间距变大重现"重叠不足"）、`min_shift_px=4`、`max_diff=9.0`（越低越严）。

---

## 4. niri 依赖（无需为非 niri 特殊处理）

- niri 相关全在 `util/niri.py`，**每个函数检测不到 niri 都优雅降级**（返回 None/False），调用方（主要是 `pin/window.py`）有 GTK 兜底。
- 本项目就是为 niri 定制的（钉图=floating、按 pid 查窗口 id、IPC 精确调窗口尺寸）。非 niri 环境下这些能力退化但不崩。
- **不要**为了"通用性"删 niri 代码，只会增加风险且没有收益。

---

## 5. 安全 / 提交注意

- 无硬编码密钥：`services/llm.py` 的 `api_key` 从环境变量 `OPENAI_API_KEY` 读。
- 只提交 `config.toml.example`，真实 `config.toml` 已在 `.gitignore`，**别提交**。
- 首次推送用 SSH（`git@github.com:tjz123psh/-Screenshot-Tool.git`），本机 `~/.ssh/id_ed25519` 已绑 GitHub 账号 `tjz123psh`。HTTPS 在非交互环境无法输密码。
- 提交前扫一遍：无 `__pycache__`、无密钥、无真实 config。

---

## 6. 验证状态

- **已真机确认可用**（本人实际使用验证）：Bug 1~5 的修复、长截图首用不再误报"重叠不足"、OCR 杂乱背景（半透明窗口透壁纸）识别——最后一项已用真实截图确认，橙色标题和正文都能正确识别，剩余错误仅是 tesseract 引擎层固有歧义（O/0、全角标点等），非背景处理问题。
- **验证方式**：GUI/交互类靠真机手动确认；OCR 类另有量化基准（渲染图 + 字符准确率打分，脚本是一次性的没入库，思路见各 bug 条目）。
- **若回归**：从对应 bug 条目的"根因"入手，不要从症状表面改。多数 bug 的根因不在表象（例：OCR 崩溃真凶是背景纹理不是颜色；涂鸦消失真凶是 niri 焦点事件不可靠不是超时太短）。

---

## 7. 提交历史备注

- 项目从 `~/projects/pngshot` 迁到 `~/Projects/pngshot`（大小写），启动器与文档路径已同步（提交 `b4bb63e`）。
- OCR 双引擎 + 预处理增强见第 2 节末尾章节。
