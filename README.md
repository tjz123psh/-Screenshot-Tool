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

## 一键安装

```sh
curl -fsSL https://raw.githubusercontent.com/tjz123psh/-Screenshot-Tool/main/install.sh | bash
```

脚本全程在用户目录内操作，**不需要 root**：

1. 把源码克隆/更新到 `~/.local/share/pngshot`
2. 在 `~/.local/bin/pngshot` 生成启动器（已内置 `PYTHONPATH` 与 `LD_PRELOAD` 修复）
3. 检查系统依赖，列出缺失项及对应的 `pacman` 安装命令
4. 提示 `~/.local/bin` 是否在 `PATH` 中

重复运行是幂等的：已安装则 `git pull` 更新后重装启动器。

> 依赖是系统级 `pacman` 包，安装脚本只做检查、不代为安装（避免脚本索要 sudo）。按脚本给出的命令自行安装即可。

## 依赖（均为 pacman 包）

```
grim wl-clipboard tesseract tesseract-data-chi_sim tesseract-data-eng
python-gobject python-cairo gtk4-layer-shell python-pillow python-opencv
python-numpy
```

翻译功能使用 PATH 上的 `opencode`（免费模型，无需登录）；不装也不影响其它功能。

一次装齐：

```sh
sudo pacman -S grim wl-clipboard tesseract tesseract-data-chi_sim \
    tesseract-data-eng python-gobject python-cairo gtk4-layer-shell \
    python-pillow python-opencv python-numpy
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

## 配置

可选的 `~/.config/pngshot/config.toml`（参见 `config.toml.example`），可覆盖大模型的模型/目标语言、OCR 引擎与预处理（`engine`、`preprocess`、`upscale`、`vision_model`）、以及长截图调优参数（`poll_ms`、`min_shift_px`、`max_diff`）。

## 命令行

| 命令 | 作用 |
|---------|------|
| `pngshot region` | 交互式区域截图 |
| `pngshot long` | 区域 → 长截图模式 |
| `pngshot pin-last` | 把当前剪贴板图片钉到桌面 |
| `pngshot debug-capture` | 抓取全屏并复制（冒烟测试） |

`region` 和 `long` 默认保存到 `~/Pictures/Screenshots` 并复制到剪贴板；可用
`--no-save` 或 `--no-copy` 分别关闭其中一项。

内部命令（由覆盖层派生，不供直接调用）：`pin-file`、`text-file`，均带 `--cleanup`。

## 长截图的限制

- **仅支持垂直滚动。** 横向移动会破坏匹配。
- **吸顶/吸底栏会重复。** 请框住滚动内容本身，避开固定栏。
- **帧间动画内容**会降低匹配得分；低置信度的帧会被丢弃，状态栏会提示你往回滚一点。

## 架构

```
pngshot/
  __main__.py        CLI（region / long / pin-last / pin-file / text-file / debug）
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
    recorder.py        定时区域采样 + 控制栏
    stitcher.py        OpenCV 垂直拼接 + 重叠复检
  services/
    clipboard.py       wl-copy / wl-paste
    ocr.py             tesseract（预处理）/ vision 双引擎 + 中文空格清理
    llm.py             opencode run（json）+ OpenAI 兜底
    saver.py           ~/Pictures/Screenshots
  util/
    niri.py            niri msg action 辅助（浮动等）
    imaging.py         PIL <-> Cairo surface
    result_win.py      OCR/翻译结果窗口
```
