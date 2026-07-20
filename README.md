# pngshot

面向 [niri](https://github.com/YaLTeR/niri) 的 Wayland 截图工具：框选区域后，在选区下方直接完成保存、复制、标注、OCR、翻译、钉图和长截图。

适用环境：Arch Linux、Wayland、niri、GTK4。项目不创建大型客户端，常驻部分只有一个轻量系统托盘和一个后台控制服务。

## 功能概览

- 区域截图：拖拽选区、移动和调整大小，工具栏贴近选区显示。
- 标注：画笔、箭头、矩形、文字，支持颜色、粗细和撤销。
- OCR：默认使用本地 Tesseract；可选用 opencode 视觉模型识别小字和低对比度内容。
- 翻译：优先复用本机 `opencode serve`，不可用时自动回退到一次性 CLI 调用。
- 长截图：手动滚动目标窗口，pngshot 连续采样并拼接垂直内容。
- 钉图：把截图作为无边框浮动窗口固定在桌面，支持缩放、移动、复制和保存。
- 系统托盘：右键菜单提供截图、长截图、钉图、保存/复制偏好、诊断、重启和退出。
- 快捷键热路径：健康服务下使用轻量 Unix socket 客户端，减少第一次加载后的启动开销；服务异常时自动拉起并回退。

## 快速开始

### 一键安装

```sh
curl -fsSL https://raw.githubusercontent.com/tjz123psh/-Screenshot-Tool/main/install.sh | bash
```

安装脚本会：

1. 在 Arch Linux 上安装缺失的系统依赖（只在确实缺包时请求 `sudo`）。
2. 把源码安装到 `~/.local/share/pngshot`。
3. 安装 `pngshot`、`pngshotctl`、应用菜单入口、图标和系统托盘服务。
4. 尝试把默认快捷键写入 `config.kdl` 实际 include 的用户自定义 `dms/keybinds.kdl`。
5. 启动并启用 `pngshot.service` 和 `pngshot-tray.service`。

安装完成后建议立即检查：

```sh
pngshotctl status
pngshotctl doctor
pngshotctl shortcuts
```

安装是幂等的，重复执行会更新源码并重新安装启动器。快捷键冲突、Niri 配置缺失或验证失败只会产生提示，不会中断 pngshot 主安装。

如果需要跳过某一步：

```sh
# 不自动安装 pacman 依赖
curl -fsSL https://raw.githubusercontent.com/tjz123psh/-Screenshot-Tool/main/install.sh \
  | PNGSHOT_SKIP_PACKAGES=1 bash

# 不修改 Niri 快捷键配置
curl -fsSL https://raw.githubusercontent.com/tjz123psh/-Screenshot-Tool/main/install.sh \
  | PNGSHOT_SKIP_SHORTCUTS=1 bash
```

### 手动安装

```sh
git clone https://github.com/tjz123psh/-Screenshot-Tool.git ~/Projects/pngshot
mkdir -p ~/.local/bin
ln -sfn ~/Projects/pngshot/scripts/pngshot ~/.local/bin/pngshot
ln -sfn pngshot ~/.local/bin/pngshotctl
pngshot region
```

手动安装不会自动安装系统依赖、systemd 服务或托盘服务；需要时请参考 `contrib/` 中的 service、desktop 和 Niri 示例文件。若源码放在其他目录，请设置 `PNGSHOT_ROOT` 后再运行启动器。

## 快捷键与 Niri/DMS

快捷键由 Niri 的 `binds {}` 注册，pngshot 本身不监听全局键盘。当前推荐绑定：

| 快捷键 | 动作 |
| --- | --- |
| `Mod+Print` | 区域截图 |
| `Mod+Shift+Print` | 区域截图并进入长截图 |
| `Mod+Ctrl+Print` | 钉住当前剪贴板图片 |

当前项目使用用户自定义的：

```text
~/.config/niri/dms/keybinds.kdl
```

`config.kdl` 已 include 该文件时，安装程序会在现有 `binds {}` 内写入带有开始/结束标记的 pngshot 区域。它不会修改 DMS 管理的 `dms/binds.kdl`，也不会覆盖其他快捷键。

快捷键自动配置是安全的、可回滚的：

- 检测到任意冲突时，不写入部分快捷键，直接提示手动配置。
- 写入前保存 `keybinds.kdl.pngshot-backup`。
- 写入后运行 `niri validate`；验证失败会恢复原文件。
- Niri 正在运行时会尝试自动 reload。

查看、安装或移除 pngshot 自己的托管区域：

```sh
pngshotctl shortcuts
pngshotctl shortcuts install
pngshotctl shortcuts remove
```

冲突或无法自动接入时，手动示例位于：

```text
~/.local/share/pngshot/contrib/niri-pngshot.kdl
```

## 使用方式

### 区域截图

按 `Mod+Print`，拖拽选区后使用选区下方工具栏：

- 确认：保存并复制（可在托盘中分别关闭保存或复制）。
- 标注：选择画笔、箭头、矩形或文字；颜色和粗细可单独选择。
- OCR：识别选区中的文字并打开可编辑结果窗口。
- 翻译：先 OCR，再使用配置的 LLM 翻译。
- 长截图：把当前选区交给滚动采集器。
- 钉图：把当前结果作为桌面浮动图片。

### 长截图

长截图是半自动流程：Wayland 不允许普通应用合成滚动事件，因此需要用户手动滚动目标窗口，pngshot 负责连续采样和拼接。

```text
Space  强制采集一帧
Enter  完成并生成长图
Esc    取消
```

为获得稳定结果：

- 只做垂直滚动，保持选区宽度不变。
- 避开吸顶、吸底和固定侧栏，否则固定内容可能重复。
- 滚动时尽量保持连续、匀速，减少动画和页面跳动。
- 选区外侧的蓝色边界用于提示采集范围，不会进入最终图片。

### 钉图

钉图窗口在 niri 中使用 floating 布局：

| 操作 | 功能 |
| --- | --- |
| 滚轮 | 缩放图片内容 |
| `Ctrl` + 滚轮 | 缩放窗口 |
| 拖动 | 移动窗口 |
| `c` / `s` | 复制 / 保存 |
| `0` | 重置缩放 |
| `q` / `Esc` | 关闭 |

### 系统托盘

托盘图标状态：

- 相机：服务就绪。
- 录制点：正在截图或采集长截图。
- 警告：服务异常。

右键菜单包含区域截图、长截图、钉图、截图后保存、截图后复制、运行诊断、重启服务和退出托盘。

## 依赖

项目面向 Arch Linux，运行依赖由 pacman 管理：

```text
git python grim wl-clipboard libnotify tesseract
tesseract-data-chi_sim tesseract-data-eng
python-gobject python-cairo gtk4-layer-shell libayatana-appindicator
python-pillow python-opencv python-numpy
```

翻译和可选视觉 OCR 需要 PATH 中存在 `opencode`；不安装它不影响截图、标注、本地 OCR 和长截图。

## 配置

复制配置示例：

```sh
mkdir -p ~/.config/pngshot
cp ~/.local/share/pngshot/config.toml.example ~/.config/pngshot/config.toml
```

配置文件为 `~/.config/pngshot/config.toml`，示例包含：

| 区块 | 常用字段 | 用途 |
| --- | --- | --- |
| `[llm]` | `provider`, `model`, `target_lang`, `serve_port` | 翻译后端、模型、目标语言和服务端口 |
| `[ocr]` | `engine`, `langs`, `preprocess`, `upscale` | Tesseract/视觉 OCR、语言和预处理 |
| `[longshot]` | `poll_ms`, `min_shift_px`, `max_diff` | 长截图采样和重叠匹配参数 |

默认配置优先使用本地 Tesseract 和 `opencode/deepseek-v4-flash-free`。如果经常翻译，可以预先启动：

```sh
opencode serve --pure --port 47823
```

pngshot 会优先复用该服务；服务不可用时自动回退到一次性调用。

## 命令行

| 命令 | 作用 |
| --- | --- |
| `pngshot region` | 区域截图 |
| `pngshot long` | 区域截图并进入长截图 |
| `pngshot pin-last` | 钉住当前剪贴板图片 |
| `pngshotctl status` | 查看后台服务状态 |
| `pngshotctl doctor` | 检查依赖、Wayland、Niri、OCR、翻译和快捷键 |
| `pngshotctl shortcuts` | 查看实际生效的 pngshot 快捷键 |
| `pngshotctl shortcuts install` | 自动安装快捷键托管区域 |
| `pngshotctl shortcuts remove` | 移除自动管理区域 |
| `pngshotctl restart` | 重启截图服务 |
| `pngshotctl logs` | 查看最近日志 |
| `pngshotctl tray` | 手动启动托盘 |

`region` 和 `long` 默认保存到 `~/Pictures/Screenshots` 并复制到剪贴板，可使用 `--no-save` 或 `--no-copy` 关闭对应行为。

## 故障排查

### 按快捷键没有反应

```sh
pngshotctl status
pngshotctl shortcuts
pngshotctl doctor
```

确认 `keybinds.kdl` 仍被 `config.kdl` include，并确认绑定中的命令是 `pngshot` 或 `pngshotctl` 的 `region`、`long`、`pin-last` 动作。快捷键由 Niri 管理，修改后需要 reload Niri。

### 第一次启动较慢

第一次调用会加载 GTK、GI、Cairo、字体和图像库；后续调用会受 Linux 页缓存和常驻截图服务帮助，通常更快。这不是截图内容缓存，也不会复用上一次选区。

### 托盘菜单为空

确认 `pngshot-tray.service` 正在运行，并使用支持传统 StatusNotifier/dbusmenu 的托盘宿主。可以先执行：

```sh
systemctl --user restart pngshot-tray.service
pngshotctl doctor
```

### 图形覆盖层异常

启动器会预加载 `/usr/lib/libgtk4-layer-shell.so`，修复 PyGObject 与 Wayland 客户端的链接顺序问题。若系统库路径不同，请确认 `gtk4-layer-shell` 已安装。

## 项目结构

```text
pngshot/
├── overlay/       区域选择、工具栏、标注和 OCR/翻译入口
├── longshot/      连续采样、滚动高亮和图像拼接
├── pin/           桌面钉图窗口
├── services/      剪贴板、保存、OCR 和 LLM 服务
├── fastctl.py     快捷键热路径客户端
├── controller.py  Unix socket 后台服务
├── shortcuts.py   Niri 快捷键检测和安全配置
└── tray.py        GTK3/Ayatana 系统托盘
```

公开安装、服务、desktop、图标和 Niri 示例文件位于 `contrib/`；真实用户配置和运行时文件不应提交到仓库。

## 许可证

当前仓库未声明开源许可证。如果你要分发或二次开发，请先确认项目所有者为仓库补充明确的 LICENSE 文件。
