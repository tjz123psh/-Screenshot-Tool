# Vellum 产品化改版验收

> 此文保留前一轮验收记录；当前界面、快捷键入口与可靠性修复以[最新复查](<REFINEMENT_REVIEW.md>)为准。

## 本轮交付

- 烟黑玻璃设置窗口：按用户参考再次精修为约638×448的原生窗口，使用真实透明底、细线图标、凹入字段和轻微立体控件。通用页提供区域截图、滚动长图、钉图和打开PNG图片；保存/复制复选框与原设置页双向同步，仍只有保存后写入。
- 独立长图查看器：完成后自动打开，适应宽度、滚轮浏览、Ctrl+滚轮缩放、键盘翻页、全文导航、复制完整图、另存为和钉图。公开命令为 vellum preview image.png。
- 设置窗口去掉偏绿平涂、大卡片和过多空白；暖金仅用于小面积强调。模式与侧栏使用统一细线图标，焦点独立于装饰阴影。微量静态颗粒用于消除8位渐变断层，不是壁纸贴图。
- 修复紧凑工具栏内距、跨行分隔线、过宽侧栏和不必要的横向滚动条。
- 随机独占0600图片交接；长图只编码一次供自动保存/复制/预览交接；剪贴板写入10秒有界等待。查看器按256行分块绘制，缓存32 MiB，避免构造整幅超长Cairo画布。

## 实际界面

以下为真实 GTK 窗口，以合成图片/默认假数据展示，不是网页效果图，不含用户密钥或页面：

- [当前通用设置](screenshots/smoke-general.png)（真实GTK透明输出叠在中性背景上展示）
- [长图查看器](screenshots/preview.png)
- [当前快捷键设置](screenshots/smoke-shortcuts.png)
- [图标16/24/32px浅深背景评审](screenshots/icons.png)（SVG浏览器栅格评审，非托盘宿主截图）

## 已运行的验证

- 全工作区、全部targets的Clippy严格检查：通过。
- 当前全工作区release测试：505通过、11默认忽略；最后的图标/复选框精修另通过181项UI测试和5项主题测试。严格Clippy与release构建通过。
- 全工作区release构建：通过，仍为4个可执行文件。
- niri真实GTK检查：工作台和三个设置页、长图查看器顶部/底部、缩放与钉图；窗口以自身WidgetPaintable/GSK截图，不拍其他桌面内容、不为截图改剪贴板。
- 显式执行 recorder::tests::live_solid_fixture_proves_recorder_ui_has_zero_sampled_pixels：通过，纯色fixture证明控制窗与高亮没有进入采样像素。
- 显式执行 surface::tests::render_the_overlay_for_review：通过；检查普通和标注工具栏的真实Cairo输出。
- 实际调用完成交接的 demo-longshot-result：父进程在1.5秒预算内返回0，独立子进程继续展示700×1932合成长图；不保存用户图片、不写剪贴板。该入口仅在 VELLUM_UI_DEMO=1 开启。
- 安装依赖查询/用户systemd不可达的隔离脚本：通过；未执行真实安装。
- 全测试中发现原有代理测试只清除HTTPS_PROXY，受宿主ALL_PROXY影响；已让测试保存、隔离并恢复全部6个代理变量，未更改任何生产代理逻辑或用户设置。

## 可复查的演示

下列命令使用构建目录，**不替换安装**。演示模式不读写真实配置或调用模型API。

~~~sh
VELLUM_UI_DEMO=1 target/release/vellum-ui panel
VELLUM_UI_DEMO=1 VELLUM_PANEL_PAGE=api target/release/vellum-ui panel
VELLUM_UI_DEMO=1 VELLUM_PANEL_PAGE=text target/release/vellum-ui panel
VELLUM_UI_DEMO=1 VELLUM_PANEL_PAGE=capture target/release/vellum-ui panel
target/release/vellum preview crates/vellum-stitch/tests/fixtures/browser-list.png
~~~

可额外设置 VELLUM_UI_SNAPSHOT=/absolute/path.png（需同时开启DEMO）保存当前窗口评审图。查看器DEMO模式F12可重新拍自己的窗口。

## 材质验证与剩余差距

设置窗口真实GTK PNG的背景样点透明度约227/255，控件样点为255/255；不是把不透明底色叫做玻璃。二次对照使用用户参考的原尺寸窗口裁图（624×440）和当前原生窗口（638×448），没有缩放界面或修饰控件。对照仅用中性背景承接PNG透明度，**没有模拟桌面模糊**。此后的原生模糊接入已经完成：本机实际公开 ext-background-effect-v1，应用借用GTK已有Wayland连接为自己的表面请求模糊，不改niri配置。生成条纹的普通GTK窗口量化测试中，对比范围从144降至3.81；不是只检查“请求已发送”。GSK导出的窗口PNG本身不含合成器后处理，因此旧对照图仍不能当作模糊后的整屏截图。详见[原生验收](NATIVE_CAPTURE_AND_BLUR.md)。

图片查看器和录制面板继续使用不透明底，保证图像与状态可读。快捷键页还修正了“需要保存”的误导：系统授权按返回结果生效，不需要另点保存。

## 快捷键现状

应用管理的标准快捷键已经实现并通过私有总线验证，包括真实分发链路（门户信号经真实 vellumctl 到达私有控制 socket）：长截图开始/结束成对、区域截图收到忙碌拒绝、钉图不继承截图输出参数，且任何失败都不会落到真实截图。

**本机当前仍无法真正绑定**：门户只公开 version 1，唯一声明实现该接口的已安装后端是 GNOME 门户，而它依赖本会话不存在的 org.gnome.Shell；niri 自身没有运行时注册快捷键的 IPC。因此设置页在状态为 unavailable 或 closed 时提供「使用兼容方式…」：点击后弹确认框，写入前备份、冲突时整组不写、验证失败回滚，绝不自动执行。详见[快捷键验证](SHORTCUTS_AND_SPEED.md)。

## 边界与后续安装

- **未覆盖已安装程序、图标缓存或用户服务，也未改快捷键/合成器配置。** 当前系统快捷键仍使用原安装版本；安装需另行确认。
- 真机环境为niri单输出；Hyprland、多屏、混合缩放尚未重新实机验收。
- 托盘菜单的外壳由宿主绘制；本轮统一入口、状态、图标资源与XDG路径，未声称能够给任意宿主菜单套GTK主题。
- 原RGB仍占3WH；32 MiB只是渲染块缓存上限。超过16384像素宽或180MP会拒绝预览，已经保存的文件不受影响。
- 原有截图标注功能保留；本轮不提供查看器内的二次标注编辑器。
