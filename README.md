# vellum

[![CI](https://github.com/tjz123psh/-Screenshot-Tool/actions/workflows/ci.yml/badge.svg)](https://github.com/tjz123psh/-Screenshot-Tool/actions/workflows/ci.yml)

面向 Arch Linux / Wayland 的原生截图工具，使用 Rust、GTK4 和 layer-shell，适配 niri 与 Hyprland。

[使用手册](<docs/USAGE.md>) · [全部文档](<docs/README.md>) · [开发与发布](<docs/DEVELOPMENT.md>) · [贡献约定](<CONTRIBUTING.md>) · [更新记录](<CHANGELOG.md>)

## 功能

- 区域截图：框选、移动和缩放选区，保存、复制与钉图。
- 标注：画笔、箭头、形状、文字、实色遮挡、马赛克、模糊、取色与撤销重做。**隐藏敏感内容请使用实色遮挡并检查范围；马赛克和模糊不能保证隐私。**
- 长截图：手动滚动、连续抓帧、自动拼接；支持往返滚动、固定页眉/页脚与局部动画。实时和离线拼接都优先避开文字选接缝。
- OCR：本地 Tesseract 离线识别，或调用兼容 API 的视觉模型。
- 翻译：使用 OpenAI 兼容接口，支持备用模型。
- 图片查看器：缩放、完整图片保存/复制、裁切编辑和 OCR，不阻塞下一次截图。
- 文字结果：可编辑、复制、翻译的浮窗；独立 OCR 关闭不再自动打开图片工作台。
- 设置与托盘：配置只在保存后生效；可管理应用快捷键授权和“开机后自启”。

## 界面预览

![Vellum 工作台示意](<docs/screenshots/compact-workbench.png>)

[图片查看器示意](<docs/screenshots/preview.png>) · [截图与标注工具栏](<docs/screenshots/compact-toolbars.png>)

截图记录了不同阶段的界面，以已安装版本为准。详细验收记录见 [文档导航](<docs/README.md>)。

## 系统要求

- 当前发行目标：Arch Linux x86_64、Wayland、GTK 4.12 及以上。
- 本机验证以 niri 为准；Hyprland 有适配代码，尚不等于已实机验收。其他桌面/发行版不承诺兼容。
- 运行库包括 gtk4、gtk4-layer-shell、grim、wl-clipboard、libnotify；本地 OCR 另需 Tesseract 与相应语言包。应用全局快捷键还依赖桌面 portal 后端支持。
- 普通用户无需 Rust 工具链，无需在本机编译。API OCR 与翻译需要自行配置兼容服务；本地 OCR 不需要外部服务。

## 安装与版本管理

### 一键安装与升级

```sh
curl -fsSL https://github.com/tjz123psh/-Screenshot-Tool/releases/latest/download/install.sh | bash
```

脚本下载发行包、校验 SHA256、检查并补齐运行库，再交给版本管理器安装；**不从源码构建，也不写入合成器快捷键配置**。Arch 缺库时会询问提权，取消或补齐失败则不开始安装。`--no-deps` 可跳过系统包处理。

旧式 0.1.x 安装迁移时，需要明确接管并备份：

```sh
curl -fsSL https://github.com/tjz123psh/-Screenshot-Tool/releases/latest/download/install.sh | bash -s -- --adopt-legacy
```

### 管理已安装版本

```sh
vellum --version
vellum build-info --json
vellum release status --json
vellum release rollback
vellum release repair
vellum release uninstall --yes
```

回退只切换程序和资源，不用旧配置覆盖新设置。截图、设置、恢复图片和用户改动不会被当作缓存删除。活动窗口或截图会使升级安全延期；服务不可达时会报告待激活状态，而不是假称已经就绪。

发行清单与故障恢复见 [发布格式](<docs/RELEASE_FORMAT.md>)；源码开发兼容入口的维护说明集中在 [开发与发布](<docs/DEVELOPMENT.md>)，不是用户默认安装方式。

## 使用

```sh
vellum region             # 区域截图
vellum long               # 开始/完成长截图
vellum pin-last           # 钉住剪贴板图片
vellum panel              # 打开工作台与设置
vellum preview image.png  # 查看图片或长截图
vellum status             # 当前截图服务状态
vellum doctor             # 环境诊断
```

“通用 → 开机后自启”需要点击“保存更改”才生效，不会中断当前截图。快捷键以系统实际授权结果为准；安装与升级不会恢复用户已移除的 niri 绑定。完整命令、快捷键、恢复图片与模型配置见 [使用手册](<docs/USAGE.md>)。

## 长截图

启动长截图、框选区域后手动垂直滚动，再次执行同一动作完成。采样期间保持选区和窗口尺寸不变，尽量避开滚动条；遇到大范围页面重排或快速跳跃时，可放慢或分段截取。

透明终端的壁纸属于视口固定背景，无法从合成后的截图还原窗口透明度；需要干净长图时请关闭终端透明度。控制条、隐私安全诊断与具体边界见 [长截图说明](<docs/USAGE.md#长截图>)。

## 配置和文件位置

配置模板为 [config.toml.example](<config.toml.example>)；默认配置在 `~/.config/vellum/config.toml`，程序版本在 `~/.local/lib/vellum`，日志在 `~/.local/state/vellum/service.log`。这些运行数据不应放进源码仓库。更多路径和环境覆盖见 [使用手册](<docs/USAGE.md#配置和文件位置>)。

## 仓库结构

| 目录或入口 | 内容 |
| --- | --- |
| [crates](<crates/>) | 7 个 Rust crate；产品代码与 Rust 单元/集成测试 |
| [contrib](<contrib/>) | 发行模板、服务、图标与显式桌面配置示例 |
| [tests](<tests/>) | 安装器、发布包和故障路径测试 |
| [tools](<tools/>) | 打包、OCR 合成回归、快捷键探测工具与浏览器样例 |
| [docs](<docs/README.md>) | 使用、开发、兼容性和历史验收导航 |
| [install-release.sh](<install-release.sh>) | 默认二进制安装入口，发布时命名为 install.sh |

## 构建与验证

默认由 Arch Linux CI 执行完整编译、strict clippy、Rust 测试、依赖审计和安装/发布故障矩阵。本地不为验证安装构建依赖。轻量检查、发布步骤与临时文件管理见 [贡献约定](<CONTRIBUTING.md>)和 [开发与发布](<docs/DEVELOPMENT.md>)。

## 架构文档

- [DESIGN](<DESIGN.md>)：当前架构、进程边界与实现取舍。
- [PERFORMANCE](<PERFORMANCE.md>)：有日期和环境说明的测量方法、实测结果与能力边界。
- [ARCHITECTURE](<ARCHITECTURE.md>)：Rust 重构时的原始需求背景，以当前代码和 DESIGN 为准。
- [文档导航](<docs/README.md>)：其余兼容性、验收和阶段记录，保持原路径便于追溯。

## 许可证

[MIT](<LICENSE>)
