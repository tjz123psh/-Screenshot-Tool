# vellum

[![CI](https://github.com/tjz123psh/-Screenshot-Tool/actions/workflows/ci.yml/badge.svg)](https://github.com/tjz123psh/-Screenshot-Tool/actions/workflows/ci.yml)

面向 Arch Linux / Wayland 的原生截图工具，使用 Rust、GTK4 和 layer-shell 构建，同时适配 **niri** 与 **Hyprland**。它是原 Python `pngshot` 的完整重构版；仓库现已只保留 vellum。

## 功能

- 区域截图：拖拽框选、移动、缩放选区，保存并复制到剪贴板。
- 标注：画笔、箭头、矩形、文字、颜色、粗细和撤销。
- 长截图：用户手动滚动，vellum 用持久 Wayland screencopy 连续抓帧并自动拼接；支持完整画布重定位、固定页眉/页脚、局部动画、半透明背景和安全回退。
- OCR：本地 Tesseract，支持简体中文和英文；会针对彩色干扰、等亮异色文字、暗淡字色、低对比度和明暗渐变自动选择预处理候选；可选视觉模型失败时回退本地 OCR。
- 翻译：优先复用本机 `opencode serve`，不可用时回退 `opencode run`，并支持免费模型池轮换。
- 钉图：无边框浮动窗口，支持移动、缩放、复制和保存。
- 系统托盘：传统 StatusNotifierItem + dbusmenu，兼容 niri/QuickShell 等托盘宿主。
- 快捷键热路径：独立的轻量 `vellumctl` 通过 Unix socket 请求常驻控制服务。

四个可执行文件职责分离：

| 可执行文件 | 作用 |
| --- | --- |
| `vellum` | 完整 CLI 与控制服务入口 |
| `vellumctl` | 快捷键使用的轻量客户端 |
| `vellum-ui` | overlay、标注、长截图、钉图和结果窗口 |
| `vellum-tray` | D-Bus 系统托盘 |

## 仓库结构

| 路径 | 内容 |
| --- | --- |
| `crates/` | 7 个 Rust crate；GTK 只存在于 `vellum-ui` |
| `contrib/` | desktop、systemd、图标及 niri/Hyprland 示例 |
| `tests/` | 安装器失败路径集成测试 |
| `tools/` | 可选的本地 OCR 合成回归工具 |
| `ARCHITECTURE.md` | Rust 重写时的原始需求契约，只作历史与验收参考 |
| `DESIGN.md` | 当前实现的架构、进程边界和技术取舍 |
| `PERFORMANCE.md` | 基准、真机验证方法和能力边界 |

## 系统要求

- Arch Linux
- Wayland
- niri 或 Hyprland；其他 Wayland 合成器可以截图，但 pin/result 窗口无法自动浮动和精确调整尺寸
- GTK 4.12 或更高版本

安装脚本可自动检查并安装以下 Arch 包：

```text
rust pkgconf gtk4 gtk4-layer-shell
grim wl-clipboard libnotify
tesseract tesseract-data-chi_sim tesseract-data-eng
```

默认的 OpenCode 翻译后端和可选视觉 OCR 还需要 PATH 中存在 `opencode`。翻译若改用 `[llm] provider = "openai"`，则该翻译路径只读取 `OPENAI_API_KEY`；视觉 OCR 仍依赖 `opencode`。不安装它不影响截图、标注、本地 OCR、钉图和长截图。

## 安装

克隆后运行可审查的安装脚本：

```sh
git clone https://github.com/tjz123psh/-Screenshot-Tool.git vellum
cd vellum
./install.sh
```

脚本会：

1. 检查 Arch 依赖，只在缺包时请求安装；
2. 执行 `cargo build --release`；
3. 将四个二进制安装到 `~/.local/bin`；
4. 安装用户级 systemd 服务、desktop 文件和图标；
5. 尝试安全配置 niri/经典 Hyprland 快捷键；Hyprland Lua 配置只输出片段，不自动修改；
6. 启动 `vellum.service` 与 `vellum-tray.service`，最后运行诊断。

不希望安装脚本处理系统包或快捷键时：

```sh
VELLUM_SKIP_PACKAGES=1 VELLUM_SKIP_SHORTCUTS=1 ./install.sh
```

也可以自定义二进制目录：

```sh
VELLUM_BIN_DIR="$HOME/bin" ./install.sh
```

> 安装脚本构建当前 checkout，不会从网络下载并执行另一个脚本，也不会维护第二份源码副本。

## 使用

```sh
vellum region             # 区域截图
vellum long               # 开始/完成长截图
vellum pin-last           # 钉住剪贴板中的图片
vellum status             # 查看控制服务状态
vellum doctor             # 完整环境诊断
vellum shortcuts          # 查看检测到的快捷键
vellum shortcuts install  # 安装默认快捷键（安全时才写入）
vellum shortcuts remove   # 移除 vellum 管理的快捷键块
vellum restart            # 重启控制服务
vellum logs               # 查看服务日志
```

默认快捷键：

| 快捷键 | 动作 |
| --- | --- |
| `Mod+Print` | 区域截图 |
| `Mod+Shift+Print` | 开始或完成长截图 |
| `Mod+Ctrl+Print` | 钉住剪贴板图片 |

快捷键由合成器注册，vellum 本身不监听全局键盘。安装器检测到冲突时不会写入一半配置；niri 修改前会备份并在写入后验证，失败则回滚。示例文件在 `contrib/`：

- `niri-vellum.kdl`
- `hyprland-vellum.conf`
- `hyprland-vellum.lua`

## 长截图

Wayland 普通应用无法安全合成全局滚轮事件，所以 vellum 不做自动滚动：

1. 执行 `vellum long` 或按 `Mod+Shift+Print`；
2. 框选目标区域；选择 overlay 会提前说明控制条会按选区外空间缩小，必要时隐藏；
3. 手动垂直滚动目标窗口；
4. 再次执行同一动作完成，或在控制面板聚焦时按 Enter；Esc 取消。

控制面板按选区外空间依次降级为完整面板、无预览 compact 面板、横向/纵向微型控制条；微型条仍保留采集状态、累计高度、取消和完成。只有连微型条都无法安全放置时才完全隐藏；正常由控制服务管理时再次按同一长截图快捷键仍可完成，direct 降级则会在采样前要求缩小选区。采样期间请保持选区和窗口尺寸不变。vellum 会处理短暂停顿、往返滚动、周期重复的列表页、突然跳回已捕获内容、固定页眉/页脚、局部动画和半透明窗口；控制面板与选区高亮不会进入结果图。长截图优先复用一条可中断的 `wlr-screencopy` 连接；协议不可用、选区跨输出或运行时失败时自动切到有 2 秒 deadline 的 `grim`，不会无限显示“采集中”却没有新帧。

透明终端的文字可以拼接，但桌面壁纸属于视口固定背景，并不存在可恢复的“下一屏壁纸”；输出中可能按每次滚动步长出现背景重复条带。需要干净长图时请临时关闭终端透明度，并尽量把滚动条/minimap 排除在选区外。这与文字滚动被误判为静止是两件事：后者由 96 列 RGB 二次确认防止，前者是合成后的 Wayland RGB 截图无法反推出窗口 alpha 的信息边界。

### 长截图隐私安全 trace

遇到“滚动很多但结果偏短”、回访后不增长或面板没有出现时，可以只为下一次长截图开启结构化 trace：

```sh
VELLUM_LONGSHOT_TRACE=1 vellum long
```

该开关会跨控制服务传给本次 `vellum-ui`，并为 session 生成随机关联 id。正常 daemon 路径写入用户私有、已有大小轮转的 `~/.local/state/vellum/service.log`；服务不可用而直接运行 UI 时写到该调用的 stderr；内容只包括生命周期状态和数值：选区/屏幕/面板几何、后端类型、捕获成功与精确重复计数、入队/出队及最大深度、每帧 `shift`/`added`/`diff`/canvas height/decision、finish 尾帧状态以及在线/离线输出高度。字段 API 不接受运行期文本，因此不会记录截图像素、窗口标题、OCR/翻译文本或后端动态错误内容。未设置为精确值 `1` 时完全关闭。

桌面通知本身也是可能被 `grim`/screencopy 拍进选区的 surface，因此采样开始前和采样期间不弹通知：选择 overlay 会按实际完成路径给出提示：daemon-managed 模式说明可再次按同一快捷键，direct 模式只指向控制面板；连微型控制条都可能放不下时，前者提示控制条可能隐藏，后者提示缩小选区。由 daemon 管理且最终没有安全位置时，状态只写入隐私安全 trace，第二次快捷键仍可完成；未连接 daemon 的 direct 模式会在可见面板标题持续显示“仅面板完成”，若面板隐藏、映射失败或实际尺寸不安全则在采样前失败，不会留下无法结束的后台采集。采集线程启动失败和拼接失败会先停止采样、关闭全部 recorder surface，再发 critical 通知。

## 配置和文件位置

主配置文件：

```text
~/.config/vellum/config.toml
```

示例见 [`config.toml.example`](config.toml.example)。如果该文件不存在，vellum 会使用内置默认值；为便于旧用户迁移，也会只读回退到 `~/.config/pngshot/config.toml`，绝不会写入旧路径。

### 翻译后端

默认主模型是 `opencode/deepseek-v4-flash-free`。vellum 先访问本机 `opencode serve` 的 HTTP 接口，服务不可用或协议失败时才回退 `opencode run`；只有模型被上游明确拒绝时才按 `fallback_models` 轮换，网络或本机错误不会把整组模型全部重试一遍。

可选的 `openai` 后端直接调用 OpenAI Chat Completions API。它不会使用 OpenCode 的免费模型别名：启用时必须同时把 `model` 改成该账号可用的 OpenAI 模型 ID，并通过环境变量 `OPENAI_API_KEY` 提供密钥。密钥不应写进仓库或 `config.toml`。

其他路径：

```text
~/.config/vellum/tray.json             托盘保存/复制偏好
~/.local/state/vellum/service.log       服务日志
~/Pictures/Screenshots/                 截图输出
$XDG_RUNTIME_DIR/vellum/control.sock    用户私有控制 socket
```

常用运行期覆盖：

| 环境变量 | 用途 |
| --- | --- |
| `VELLUM_RUNTIME_DIR` | 覆盖 runtime/socket 目录，主要用于测试隔离 |
| `VELLUM_RENDERER` | 覆盖 overlay 默认使用的 Cairo GSK renderer |
| `VELLUM_TRACE=1` | 输出 `vellum-ui` 启动阶段打点 |
| `VELLUM_OCR_TRACE=1` | 输出 OCR 候选、PSM、耗时和置信度（不输出识别文本） |
| `VELLUM_LONGSHOT_TRACE=1` | 输出长截图生命周期、队列和拼接数值 trace；不记录像素或运行期文本 |
| `VELLUM_LONGSHOT_BACKEND=grim` | 禁用长截图 screencopy，强制使用有界 grim 回退 |
| `VELLUM_ICON_PATH` | 覆盖托盘图标目录 |

## 构建与验证

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo audit --no-yanked
cargo bench -p vellum-stitch
cargo bench -p vellum-ipc
bash tests/install-dependency-query.sh

# 可选的本地 OCR 集成回归；需要 Pillow、Tesseract 中英语言包和 CJK 字体
python3 tools/ocr-regression.py
```

当前实现包含 7 个 workspace crate、4 个安装二进制和 300 余项 Rust 测试，其中 2 项需 Wayland 真机显式运行。101 帧、900×700 的长截图基准约为 0.15 秒，Unix socket 的 ping/status 往返 p50 约为 0.04 毫秒；方法和完整数据见 [`PERFORMANCE.md`](PERFORMANCE.md)。架构和取舍见 [`DESIGN.md`](DESIGN.md)。

## 架构文档

- [`DESIGN.md`](DESIGN.md)：进程边界、依赖选择、合成器抽象、IPC、截图和 UI 设计。
- [`PERFORMANCE.md`](PERFORMANCE.md)：长截图、IPC、OCR 与启动延迟的测量方法和结果。
- [`ARCHITECTURE.md`](ARCHITECTURE.md)：发起 Rust 重构时使用的原始需求契约；其中个别 niri-only 或旧命名描述已被后续双合成器实现取代，以当前代码和 `DESIGN.md` 为准。

## 许可证

[MIT](LICENSE)
