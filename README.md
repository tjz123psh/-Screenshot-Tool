# vellum

[![CI](https://github.com/tjz123psh/-Screenshot-Tool/actions/workflows/ci.yml/badge.svg)](https://github.com/tjz123psh/-Screenshot-Tool/actions/workflows/ci.yml)

面向 Arch Linux / Wayland 的原生截图工具，使用 Rust、GTK4 和 layer-shell 构建，同时适配 **niri** 与 **Hyprland**。它是原 Python `pngshot` 的完整重构版；仓库现已只保留 vellum。

## 功能

- 区域截图：拖拽框选、移动、缩放选区，保存并复制到剪贴板。
- 标注：画笔、箭头、矩形、文字、颜色、粗细和撤销。
- 长截图：用户手动滚动，vellum 连续抓帧并自动拼接；支持固定页眉/页脚、局部动画、半透明背景和离线路径重建。
- OCR：本地 Tesseract，支持简体中文和英文；可选视觉模型，失败时回退本地 OCR。
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

翻译和可选视觉 OCR 还需要 PATH 中存在 `opencode`。不安装它不影响截图、标注、本地 OCR、钉图和长截图。

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
2. 框选目标区域；
3. 手动垂直滚动目标窗口；
4. 再次执行同一动作完成，或在控制面板聚焦时按 Enter；Esc 取消。

采样期间请保持选区和窗口尺寸不变。vellum 会处理短暂停顿、往返滚动、固定页眉/页脚、局部动画和半透明窗口；控制面板与选区高亮不会进入结果图。

## 配置和文件位置

主配置文件：

```text
~/.config/vellum/config.toml
```

示例见 [`config.toml.example`](config.toml.example)。如果该文件不存在，vellum 会使用内置默认值；为便于旧用户迁移，也会只读回退到 `~/.config/pngshot/config.toml`，绝不会写入旧路径。

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
| `VELLUM_ICON_PATH` | 覆盖托盘图标目录 |

## 构建与验证

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo audit --no-yanked
cargo bench -p vellum-stitch
cargo bench -p vellum-ipc
```

当前实现包含 7 个 workspace crate、4 个二进制和 216 项测试。101 帧、900×700 的长截图基准约为 0.15 秒，Unix socket 的 ping/status 往返 p50 约为 0.04 毫秒；方法和完整数据见 [`PERFORMANCE.md`](PERFORMANCE.md)。架构和取舍见 [`DESIGN.md`](DESIGN.md)。

## 架构文档

- [`DESIGN.md`](DESIGN.md)：进程边界、依赖选择、合成器抽象、IPC、截图和 UI 设计。
- [`PERFORMANCE.md`](PERFORMANCE.md)：长截图、IPC、OCR 与启动延迟的测量方法和结果。
- [`ARCHITECTURE.md`](ARCHITECTURE.md)：发起 Rust 重构时使用的原始需求契约；其中个别 niri-only 或旧命名描述已被后续双合成器实现取代，以当前代码和 `DESIGN.md` 为准。

## 许可证

[MIT](LICENSE)
