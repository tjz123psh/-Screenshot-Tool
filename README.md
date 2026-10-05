# vellum

[![CI](https://github.com/tjz123psh/-Screenshot-Tool/actions/workflows/ci.yml/badge.svg)](https://github.com/tjz123psh/-Screenshot-Tool/actions/workflows/ci.yml)

面向 Arch Linux / Wayland 的原生截图工具，使用 Rust、GTK4 和 layer-shell 构建，同时适配 **niri** 与 **Hyprland**。它是原 Python `pngshot` 的完整重构版；仓库现已只保留 vellum。

## 功能

- 区域截图：拖拽框选、移动、缩放选区，保存并复制到剪贴板。
- 标注：紧凑工具行与按需属性条；画笔、箭头、矩形、椭圆、文字、实色遮挡、马赛克、模糊、取色、颜色、大小、撤销和重做。**实色遮挡（H）**用置顶纯黑矩形覆盖像素，适合遮住敏感内容；仍需核对覆盖范围。其中马赛克与模糊读取原始截图而不是已标注的合成结果，但只提供**视觉弱化，不保证敏感内容无法辨认或推测**，不能代替不透明实色遮挡。**取色**（`i`）点一下即取该像素的颜色：色值以 `#RRGGBB` 进剪贴板，同时成为当前标注颜色，反馈就是「颜色」按钮上的色块当场变色；它同样取原始截图的像素，所以已经被画过的地方取到的仍是屏幕原来的颜色。
- 长截图：用户手动滚动，vellum 用持久 Wayland screencopy 连续抓帧并自动拼接；支持完整画布重定位、固定页眉/页脚、局部动画、半透明背景和安全回退。
- OCR：两种引擎二选一——**内置本地 Tesseract**（离线、简体中文+英文，会针对彩色干扰、等亮异色文字、暗淡字色、低对比度和明暗渐变自动选择预处理候选）或 **API 视觉模型**。
- 翻译：只走 OpenAI 兼容的模型 API（OpenAI、DeepSeek、OpenRouter、Ollama、vLLM…），支持模型轮换。
- 设置面板：托盘主点击、菜单「打开工作台」或 `vellum panel` 打开。按参考图整理为烟灰底、暖金细节、紧凑模式按钮和对齐设置行；包括全局快捷键授权入口，配置只在保存后写入。
- 图片查看器：长截图完成后自动打开独立深色查看窗口，默认适应宽度并从顶部展示；滚轮浏览、缩放、全文导航、复制、另存为和钉图，不占用下一次截图。
- 结果窗口：OCR / 翻译结果自带标题栏，可拖动、可编辑、可复制或再次翻译。
- 钉图：无边框浮动窗口，支持移动、缩放、复制和保存。
- 系统托盘：传统 StatusNotifierItem + dbusmenu，兼容 niri/QuickShell 等托盘宿主。
- 快捷键热路径：独立的轻量 `vellumctl` 通过 Unix socket 请求常驻控制服务。

四个可执行文件职责分离：

| 可执行文件 | 作用 |
| --- | --- |
| `vellum` | 完整 CLI 与控制服务入口 |
| `vellumctl` | 快捷键使用的轻量客户端 |
| `vellum-ui` | overlay、标注、长截图、钉图、结果窗口和设置面板 |
| `vellum-tray` | D-Bus 系统托盘 |

## 界面预览

![Vellum 工作台 · 800×560](<docs/screenshots/compact-workbench.png>)

[长图查看器](<docs/screenshots/preview.png>) · [快捷键设置](<docs/screenshots/native-shortcuts.png>) · [紧凑截图与标注工具栏](<docs/screenshots/compact-toolbars.png>) · [品牌图标](<contrib/icons/ai.vellum.svg>) · [功能完整性与最新界面验收](<docs/COMPACT_UI_AND_FEATURE_AUDIT.md>) · [长截图可靠性复查](<docs/REFINEMENT_REVIEW.md>) · [第一批可靠性修复](<docs/RELIABILITY_PHASE1.md>) · [会话编辑与统一输出](<docs/WORKFLOW_PHASE2.md>)

设置窗口现在会自动探测标准 `ext-background-effect-v1`：支持的桌面直接由合成器模糊背景，无需手改窗口规则；不支持时保留更深的透明底。图片和录制区域仍不透明。已完成[真实高速采集与模糊量化验证](<docs/NATIVE_CAPTURE_AND_BLUR.md>)。

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

- 当前交付目标：Arch Linux x86_64、Wayland、GTK 4.12及以上。
- 本机验证以 niri 为准；Hyprland 有适配代码但本轮未实机验收，其他桌面/发行版不承诺兼容。版本说明必须保留这个边界。
- 运行依赖包括 gtk4、gtk4-layer-shell、xdg-desktop-portal、grim、wl-clipboard、libnotify、tesseract 及所需语言包。
- Rust、pkgconf、Python仅用于源码构建/打包；二进制发行包不要求用户本机编译。

翻译与API视觉OCR需要配置自己的兼容接口；本地Tesseract识别不需要外部服务。普通导出与OCR使用遮挡合成后的成品，不附带原始编辑工程。

## 安装与版本管理

### 使用已校验的二进制包

仅安装信任来源的包。校验和可以检查损坏与一致性，但不能单独证明发布者身份；正式候选还应核对固定标签与构建来源。**本工作树生成的dirty/未知来源包明确标为开发包，不等于已经公开发布的正式版本。**

在解压后的发行包目录执行：

```sh
./install.sh
```

旧式安装迁移需要明确选择接管，并先备份。此通道只接管不含新构建身份的旧程序；带新身份的现代散装二进制会被拒绝，不能伪装成可回退旧版：

```sh
./install.sh --adopt-legacy
```

安装先准备整套程序与资源，再切换当前版本。活动窗口、截图或独立旧后台进程会导致安全延期；用户服务不可达时只报告“已安装待激活”，不会声称托盘已就绪。按报告提供的管理程序及参数，在保存关闭工作后执行修复/激活。

### 管理已安装版本

```sh
vellum build-info --json
vellum release status --json
vellum release install --bundle /path/to/verified-bundle
vellum release rollback
vellum release repair
vellum release uninstall --yes
vellum support --json
vellum support --output support.json
```

回退只切程序与资源，不拿陈旧配置覆盖用户新设置。卸载只清理仍能验证归属的入口和程序；旧式备份、在用版本、用户改动及未知文件保留并列出原因，截图、设置、恢复图片和桌面快捷键不动。release status描述安装事务，vellum status描述当前截图服务，两者不是一回事。

### 开发者从源码安装

本段针对仓库源码根目录，和发行包内的小安装入口不同：

```sh
./install.sh
# 仅当确需迁移已确认属于Vellum的旧式安装：
VELLUM_ADOPT_LEGACY=1 ./install.sh
# 依赖已核对齐全时可跳过系统装包：
VELLUM_SKIP_PACKAGES=1 VELLUM_SKIP_CLEANUP=1 ./install.sh
```

源码入口保留构建缓存，先校验四个程序构建身份，再委托同一版本管理器。依赖检查失败会在构建和安装前停止；提权取消不重试。安装器不再写入任何合成器快捷键，传统绑定必须另行明确操作。

远程源码通道现在要求完整固定提交，不再默认下载移动的main分支：

```sh
VELLUM_SOURCE_REF=<40位提交SHA> bash install-remote.sh
```

只有包含版本化安装器的提交才会运行；临时源码会清理，安装版本保存在用户级版本根。该开发通道不代替正式二进制发行包。

详情见[发布格式](<docs/RELEASE_FORMAT.md>)与[连续执行计划](<docs/PRODUCT_EXECUTION_PLAN.md>)。


## 使用

```sh
vellum region             # 区域截图
vellum long               # 开始/完成长截图
vellum pin-last           # 钉住剪贴板中的图片
vellum panel              # 打开工作台与偏好设置
vellum preview image.png  # 打开图片/长图查看器
vellum recover list       # 列出交接失败后保留的恢复图片
vellum recover open ID    # 按编号重新打开，仍保留恢复副本
vellum recover discard ID # 明确删除指定恢复副本，不删除另存的图片
vellum status             # 查看控制服务状态
vellum doctor             # 完整环境诊断
vellum shortcuts          # 查看应用全局快捷键的实际状态
vellum shortcuts enable   # 启用并申请系统授权
vellum shortcuts disable  # 停用应用绑定，不动旧桌面配置
vellum shortcuts configure # 打开系统快捷键配置（需要接口版本2）
vellum restart            # 重启控制服务
vellum logs               # 查看服务日志
```

建议组合键（实际以系统授权返回为准）：

| 快捷键 | 动作 |
| --- | --- |
| `Mod+Print` | 区域截图 |
| `Mod+Shift+Print` | 开始或完成长截图 |
| `Mod+Ctrl+Print` | 钉住剪贴板图片 |

Vellum 通过 XDG GlobalShortcuts 注册并处理全局按键，首次在应用“快捷键”页启用，系统可能要求授权。应用显示系统实际绑定的组合键，不把建议值当作已生效。**桌面后端必须支持此接口；仅能枚举接口不等于能成功绑定。** 本机 niri + GNOME portal 的真实绑定被拒绝，详见[兼容性与高速滚动记录](<docs/SHORTCUTS_AND_SPEED.md>)。

旧配置不会自动删除。`vellum shortcuts legacy-list` 只读检查旧绑定；`install` / `remove` 子命令仅保留为显式传统兼容入口。版本化安装器不再自动写旧配置，即使环境中残留旧安装开关也不会恢复已移除的绑定。兼容示例在 `contrib/`：

- `niri-vellum.kdl`
- `hyprland-vellum.conf`
- `hyprland-vellum.lua`

## 长截图

Wayland 普通应用无法安全合成全局滚轮事件，所以 vellum 不做自动滚动：

1. 执行 `vellum long`、点击工作台「滚动长图」，或使用已授权/配置的长截图快捷键；
2. 框选目标区域；选择 overlay 会提前说明控制条会按选区外空间缩小，必要时隐藏；
3. 手动垂直滚动目标窗口；
4. 再次执行同一动作完成，或在控制面板聚焦时按 Enter；Esc 取消。
5. 完成后按原有偏好保存/复制，并自动打开独立图片查看器。普通滚轮上下浏览，Ctrl+滚轮缩放，Shift+滚轮横移；`+`/`-` 缩放、`1` 原始大小、`Ctrl+0` 适应宽度，`Home`/`End` 与 `PageUp`/`PageDown` 导航。右侧全文概览可点击、拖动定位；复制和另存为始终输出完整图片，而非屏幕上可见的一部分。关闭查看器前可继续发起下一次截图。

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

### 设置面板

托盘主点击或右键菜单选「打开工作台」，也可以执行 `vellum panel`。默认展示工作台，提供区域截图、长截图和钉图入口；设置分为截图输出、识别翻译、模型接口三个页面。`Ctrl+0` 返回工作台，保留 `Ctrl+1` 模型接口、`Ctrl+2` 识别翻译、`Ctrl+3` 截图输出；`Ctrl+S` 保存、`Esc` 关闭。从工作台开始截图前会先关闭窗口，有未保存更改时先询问，避免工作台入镜或默默丢失设置。

- **模型接入**
  - 接口：根地址（`base_url`）、密钥、密钥所在的环境变量名、单次请求超时、HTTP 代理；「测试连接」会请求 `/models` 验证地址与密钥。密钥优先取这里填的值，其次取指定的环境变量，最后回退 `VELLUM_API_KEY` / `OPENAI_API_KEY`；本机地址（如 Ollama）可以完全不填密钥。
  - 模型：点「获取模型」从接口读回真实模型名，填进「翻译模型」和「OCR 视觉模型」两个选择器。选择器**既能点选也能手写**：右侧按钮展开可搜索的列表，点一行即填入；手写的名字如果不在获取到的列表里，面板会立刻标红提示（这是最常见的一类 404 来源）。
- **识别与翻译**：目标语言、备用模型；OCR 引擎二选一（内置 Tesseract / API 视觉模型）、Tesseract 语言包、图像预处理、放大倍数、视觉模型超时。
- **截图与输出**：「截图后保存 / 复制」开关。较长页面可上下滚动，底部保存操作固定可达。

代理那一项：接口在墙外（OpenAI、Google）时必须填，例如 `http://127.0.0.1:7890`；留空则依次读 `HTTPS_PROXY` / `ALL_PROXY` / `HTTP_PROXY`，填 `none` 表示强制直连。注意 systemd 启动的服务看不到你 shell 里 export 的变量，所以写进配置文件（面板）比依赖环境变量可靠。

保存后面板会把配置写到下面的配置文件（原子替换、权限 0600），**下一次**截图或长截图自动生效，不需要重启服务。

### 模型接口

翻译与 API 视觉 OCR 都走同一个 OpenAI 兼容接口，即 `POST {base_url}/chat/completions`：

- 翻译：把待翻译文本作为一条 user 消息发出，只有被上游明确拒绝（4xx/5xx 且服务端说明原因）时才按 `fallback_models` 换下一个模型；网络或本机错误不会把整组模型重试一遍。结果窗口底部会显示这次用的是哪个模型（原文已是目标语言时也会说明）。
- API OCR：把选区 PNG 以 data URL 放进消息里，要求模型"只输出图片中的文字"。模型留空时复用 `[llm].model`。

密钥建议放在环境变量里（`VELLUM_API_KEY`，或配置中 `api_key_env` 指定的名字）；直接写进 `config.toml` 也可用，但文件是明文。

**两种常见失败与对策**：

| 现象 | 原因与做法 |
|---|---|
| `连接失败：连接 <host> 超时（N 秒）` | 地址不可达（墙、断网、写错）。接口在墙外时在面板「HTTP 代理」里填本地代理，例如 `http://127.0.0.1:7890`；也可以换一个可达的 OpenAI 兼容服务（DeepSeek、OpenRouter、阿里云百炼等在国内可直连） |
| `接口可达，但模型 X 不在返回的 N 个模型里` | 地址和密钥都对，模型名写错了。例如 Google 的 OpenAI 兼容端点（`.../v1beta/openai`）用 `gemini-2.5-flash` 这类模型名，填 `gpt-4o-mini` 只会在真正翻译时 404 |

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
| `VELLUM_TRACE` | 输出 `vellum-ui` 启动阶段打点；**变量存在即启用**（写成 `0` 也生效） |
| `VELLUM_OCR_TRACE=1` | 输出 OCR 候选、PSM、耗时和置信度（不输出识别文本）；**必须精确为 `1`** |
| `VELLUM_LONGSHOT_TRACE=1` | 输出长截图生命周期、队列和拼接数值 trace；不记录像素或运行期文本；**必须精确为 `1`** |
| `VELLUM_LONGSHOT_BACKEND=grim` | 禁用长截图 screencopy，强制使用有界 grim 回退 |
| `VELLUM_ICON_PATH` | 覆盖托盘图标目录 |
| `VELLUM_API_KEY` | 模型接口密钥（默认读取的名字；也可在 `[api] api_key_env` 里改成别的变量） |
| `OPENAI_API_KEY` | 后备密钥变量，兼容只设置了它的旧配置 |

## 构建与验证

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo audit --no-yanked
cargo bench -p vellum-stitch
cargo bench -p vellum-ipc
bash tests/install-dependency-query.sh
bash tests/install-systemd-unreachable.sh

# 可选的本地 OCR 集成回归；需要 Pillow、Tesseract 中英语言包和 CJK 字体
python3 tools/ocr-regression.py
```

当前实现包含 7 个 workspace crate、4 个安装二进制。产品化改版后全工作区 release 测试为 **505 通过、11 项默认忽略**（需显式图形场景/评审输出）。101 帧、900×700 的长截图基准为 0.13~0.15 秒，Unix socket 的 ping/status 往返 p50 为 0.037~0.056 毫秒（2026-09-18 复测 6 次，见 [`PERFORMANCE.md`](PERFORMANCE.md)）。架构和取舍见 [`DESIGN.md`](DESIGN.md)。

## 架构文档

- [`DESIGN.md`](DESIGN.md)：进程边界、依赖选择、合成器抽象、IPC、截图和 UI 设计。
- [`PERFORMANCE.md`](PERFORMANCE.md)：长截图、IPC、OCR 与启动延迟的测量方法和结果。
- [`ARCHITECTURE.md`](ARCHITECTURE.md)：发起 Rust 重构时使用的原始需求契约；其中个别 niri-only 或旧命名描述已被后续双合成器实现取代，以当前代码和 `DESIGN.md` 为准。

## 许可证

[MIT](LICENSE)
