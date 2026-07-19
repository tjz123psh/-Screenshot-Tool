# pngshot

面向 niri 的 Wayland 区域截图工具。框选一块区域后，紧贴选区下方会浮出一排工具栏，可直接复制、保存、标注、OCR、大模型翻译、钉到桌面，或进行长截图（滚动拼接）。

适用环境：Arch Linux + niri（Wayland），GTK4 + gtk4-layer-shell。

## 功能

- **区域截图** —— 拖拽框选，8 个缩放手柄，可拖动移动选区。
- **选区下方工具栏** —— 像素级精确，因为整个选区阶段是一整块全屏 `wlr-layer-shell` 覆盖层（而非独立窗口）。
- **确认 → 剪贴板**（`wl-copy -t image/png`）、**保存**、**取消**。
- **OCR** —— 默认走 tesseract（本地、快、无需联网），会先对截图做放大/灰度/深色主题自动反色的预处理，明显改善屏幕小字的识别。也可在配置里切到 `engine = "vision"`，用 opencode 视觉模型识别（小字、中英混排、低对比度更强，标点更准），失败会自动回退 tesseract。结果显示在可编辑窗口中。
- **翻译** —— 走本机 opencode 免费模型（`opencode run --format json`），也支持通过 `OPENAI_API_KEY` 使用 OpenAI 兼容接口。
- **标注** —— 画笔 / 箭头 / 矩形 / 文字，可循环切换颜色与粗细，支持撤销。文字通过 Pango 渲染，中文正常显示。
- **桌面钉图** —— 无边框浮动窗口（在 niri 中「置顶」即浮动）：
  - 滚轮 = 缩放图片内容（窗口大小不变，以光标为锚点）
  - Ctrl + 滚轮 = 缩放窗口本身
  - 任意位置拖动 = 移动窗口
  - `c` 复制，`s` 保存，`0` 重置缩放，`q`/`Esc` 关闭，右键菜单
- **长截图** —— 半自动。Wayland 禁止合成滚动事件，所以由你手动滚动目标窗口，pngshot 按定时采样选区，并用 OpenCV 模板匹配拼接各帧。`Space` 强制取一帧，`Enter` 完成，`Esc` 取消。
- **可靠的快捷键服务** —— 轻量后台服务确认每次调用；`pngshotctl` 的截图热键走精简 socket 客户端，健康服务下不加载完整 CLI，异常时自动拉起并回退；动作启动失败会显示桌面通知并记录日志。
- **系统托盘** —— 常驻相机图标显示就绪/截图中/异常；菜单直接提供区域截图、长截图、钉图、保存/复制偏好、诊断和重启。

## 一键安装

```sh
curl -fsSL https://raw.githubusercontent.com/tjz123psh/-Screenshot-Tool/main/install.sh | bash
```

脚本会把程序安装在用户目录；在 Arch Linux 上若检测到缺失的系统依赖，会仅在装包阶段请求一次 `sudo`：

1. 用 `pacman` 自动安装实际缺失的运行依赖（已安装的包不会重复安装）
2. 把源码克隆/更新到 `~/.local/share/pngshot`
3. 安装 `pngshot` / `pngshotctl` 启动器（内置 `PYTHONPATH` 与 `LD_PRELOAD` 修复）
4. 安装应用菜单入口、应用图标和系统托盘状态图标
5. 安装并启动 systemd 用户服务与系统托盘
6. 再次检查运行环境，并提示 `~/.local/bin` 是否在 `PATH` 中

重复运行是幂等的：已安装则 `git pull` 更新后重装启动器。

> 不希望脚本自动装包时，可使用 `curl ... | PNGSHOT_SKIP_PACKAGES=1 bash`；脚本仍会检查环境并列出缺失项。非 Arch 系统没有 `pacman` 时也会自动跳过装包步骤。

## 依赖（均为 pacman 包）

```
grim wl-clipboard libnotify tesseract tesseract-data-chi_sim tesseract-data-eng
python-gobject python-cairo gtk4-layer-shell libayatana-appindicator
python-pillow python-opencv python-numpy
```

翻译功能使用 PATH 上的 `opencode`（免费模型，无需登录）；不装也不影响其它功能。

一次装齐：

```sh
sudo pacman -S grim wl-clipboard libnotify tesseract tesseract-data-chi_sim \
    tesseract-data-eng python-gobject python-cairo gtk4-layer-shell \
    libayatana-appindicator python-pillow python-opencv python-numpy
```

## 手动安装

不想用一键脚本，也可以手动来：

```sh
git clone https://github.com/tjz123psh/-Screenshot-Tool.git ~/.local/share/pngshot
ln -s ~/.local/share/pngshot/scripts/pngshot ~/.local/bin/pngshot
# scripts/pngshot 默认从 ~/Projects/pngshot 找源码，若放在别处需设 PNGSHOT_ROOT：
#   export PNGSHOT_ROOT=~/.local/share/pngshot
pngshot region
```

> **关于 LD_PRELOAD**：PyGObject 会在 gtk4-layer-shell 之前链接 libwayland，导致 layer surface 失效。启动器通过预加载 `/usr/lib/libgtk4-layer-shell.so` 修复此问题，详见 <https://github.com/wmww/gtk4-layer-shell/blob/main/linking.md>。

## niri 集成

`contrib/niri-pngshot.kdl` 提供了窗口规则（保持钉图窗口整洁）和键位示例。脚本不会自动应用，请自行把需要的部分合并进 `~/.config/niri/config.kdl`。

推荐键位：

| 按键 | 动作 |
|-----|--------|
| `Print` | `pngshot region` |
| `Shift+Print` | `pngshot long` |
| `Mod+Print` | `pngshot pin-last` |

`pngshot` 与 `pngshotctl` 都会走同一套确认协议。新配置推荐使用 `pngshotctl`，便于区分“向服务发送动作”和内部一次性窗口进程。

快捷键由 Niri 的 `binds {}` 负责注册，pngshot 不会抢占全局键盘，也不会自动改写你的 Niri 配置。要改按键，只需编辑实际被 include 的 `.kdl` 文件，把 `spawn-sh`（或 `spawn`）后面的动作保持为 `region`、`long`、`pin-last` 之一；修改后按你的 Niri 配置方式 reload。查看当前实际生效的 pngshot 绑定可以运行：

```sh
pngshotctl shortcuts
```

### 服务状态

安装后 `pngshot.service` 由 systemd 用户会话监管。即使服务没有运行，截图命令也会先自动拉起并重试；服务仍不可用时才回退原来的直接启动路径，因此不会因为后台服务故障而完全失去截图能力。

```sh
pngshotctl status          # 人类可读状态
pngshotctl status --json   # Niri/Waybar/QuickShell 状态模块
pngshotctl doctor          # 依赖、环境、OCR 语言、快捷键诊断
pngshotctl restart         # 重启服务
pngshotctl logs            # 最近 50 行日志
pngshotctl shortcuts       # 列出 Niri 中实际配置的截图快捷键
pngshotctl tray            # 手动启动托盘（通常由 systemd 自动启动）
```

托盘图标会按状态切换：相机（就绪）、录制点（正在截图）、警告（服务异常）。若使用自定义状态模块，也可按 JSON 的 `state` 字段显示：`idle`、`busy`、`stopped`。

## 配置

可选的 `~/.config/pngshot/config.toml`（参见 `config.toml.example`），可覆盖大模型的模型/目标语言、OCR 引擎与预处理（`engine`、`preprocess`、`upscale`、`vision_model`）、以及长截图调优参数（`poll_ms`、`min_shift_px`、`max_diff`）。若 `serve_port` 上已有 `opencode serve`，翻译会直接复用其 HTTP API；否则自动回退一次性 CLI，不需要手动切换。

经常使用免费模型翻译时，可在登录会话中启动 `opencode serve --pure --port 47823`，后续翻译会复用已加载的服务；不启动也不影响功能。

## 命令行

| 命令 | 作用 |
|---------|------|
| `pngshot region` | 交互式区域截图 |
| `pngshot long` | 区域 → 长截图模式 |
| `pngshot pin-last` | 把当前剪贴板图片钉到桌面 |
| `pngshot tray` | 手动启动系统托盘（通常无需手动执行） |
| `pngshot status --json` | 输出后台服务状态，供状态栏读取 |
| `pngshot doctor` | 检查截图、OCR、通知、翻译和快捷键环境 |
| `pngshot restart` | 重新启动后台服务 |
| `pngshot logs` | 查看最近服务与动作日志 |
| `pngshot debug-capture` | 抓取全屏并复制（冒烟测试） |

`region` 和 `long` 默认保存到 `~/Pictures/Screenshots` 并复制到剪贴板；可用
`--no-save` 或 `--no-copy` 分别关闭其中一项。

内部命令（由覆盖层派生，不供直接调用）：`pin-file`、`text-file`，均带 `--cleanup`。

## 长截图的限制

- 采集中会在选区**外侧**显示蓝色边界；边界不遮挡内容，也不会进入最终长图。
- **仅支持垂直滚动。** 横向移动会破坏匹配。
- **吸顶/吸底栏会重复。** 请框住滚动内容本身，避开固定栏。
- **帧间动画内容**会降低匹配得分；采集器会保留连续中间帧，确实无法建立重叠时状态栏才会提示调整。

## 架构

```
pngshot/
  __main__.py        CLI（region / long / pin-last / pin-file / text-file / debug）
  fastctl.py         快捷键热路径的精简 Unix socket 客户端
  controller.py      Unix socket 后台服务、动作确认、自愈与失败通知
  tray.py            GTK3/Ayatana 系统托盘与传统 dbusmenu 右键菜单
  diagnostics.py     Wayland/Niri/截图/OCR/翻译环境检查
  capture.py         grim 封装（全屏 / 指定输出 / 区域）
  config.py          ~/.config/pngshot/config.toml 加载器
  overlay/           阶段一：全屏 layer-shell 覆盖层
    surface.py         layer-shell 窗口、Cairo 渲染、事件分发
    selector.py        选区矩形 + 缩放/移动状态机
    toolbar.py         区域工具栏 + 标注工具栏（Pango 文字）
    annotate.py        画笔/箭头/矩形/文字笔画 + 烘焙进图片
    model.py           Rect、Mode、手柄几何
    app.py             捕获 → 覆盖层 → 动作流水线；长截图交接
  pin/window.py      桌面钉图浮动窗口（内容/窗口缩放、移动）
  longshot/
    recorder.py        后台连续采样 + 有序队列 + 控制栏
    highlight.py       采集区外侧 layer-shell 高亮边界
    stitcher.py        OpenCV 垂直拼接 + 重叠复检
  services/
    clipboard.py       wl-copy / wl-paste
    ocr.py             tesseract（预处理）/ vision 双引擎 + 中文空格清理
    llm.py             opencode serve 快速通道 / CLI 回退 + OpenAI 兜底
    saver.py           ~/Pictures/Screenshots
  util/
    niri.py            niri msg action 辅助（浮动等）
    imaging.py         PIL <-> Cairo surface
    result_win.py      OCR/翻译结果窗口
```
