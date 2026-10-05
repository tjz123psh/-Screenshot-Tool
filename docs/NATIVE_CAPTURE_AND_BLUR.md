# 原生高速采集与真实背景模糊验收

## 1. 高速移动画面经过真实采集链路

入口：[原生高速测试](../crates/vellum-ui/src/recorder_live_test.rs)。该测试仅在显式调用时运行，短暂用生成的画面覆盖唯一输出，不保存任何桌面图片。

链路为 GTK合成页面 → niri → 持久wlr-screencopy连接 → 实际有界队列 → 原在线拼接器 → 排空与尾帧收口。不是把预先截图直接送入拼接器。

当前niri 26.04、单输出、scale=1的实测：

| 项目 | 结果 |
|---|---:|
| 视口 | 600×700 |
| 演示更新间隔 / 最大单次位移 | 16 ms / 160 px |
| 加速、暂停、反向、再向下的演示更新时段 | 约0.81秒 |
| 成功采集 | 52 |
| 丢弃的逐字节重复帧 | 4 |
| 进入拼接的帧 | 48 |
| 采集失败 / 拼接拒绝 | 0 / 0 |
| 队列最大深度 | 1 |
| 输出 | 600×4068 |
| 相对生成源图的最大颜色通道差 | 0 |
| 非一致颜色通道数 | 0 |

控件在采集期间确实显示在选区外；输出像素全等也排除了控件污染。首次测试误在结束关闭窗口后检查“仍可见”，已将夹具修正为记录采集期间可见，不改动采集实现。

启用libwayland互操作后重新执行了这条链路，结果仍为0像素差异。此测试不代表任意速度、任意小选区或无重叠跳转都能完整恢复；漏采与重复卡片仍使用[高速保护回归](../crates/vellum-stitch/tests/fast_scroll.rs)。

## 2. 合成器真实背景模糊

实际Wayland registry包含 ext_background_effect_manager_v1。niri [官方窗口效果说明](https://raw.githubusercontent.com/niri-wm/niri/main/docs/wiki/Window-Effects.md)明确说明26.04支持应用通过此协议直接请求模糊，包括客户端自己的圆角区域，无须手工窗口规则。

实现位于[原生模糊模块](../crates/vellum-ui/src/background_blur.rs)：

- 只用于设置窗口；录制面板、图片内容和文字结果不因玻璃效果被透明化。
- 借用GTK现有Wayland连接，guest backend不拥有连接；不覆盖GDK监听器、不替GTK读socket、不手动提交或销毁GTK的wl_surface。
- 用自己的事件队列异步获取能力，确认Blur位后创建效果；区域按表面逻辑尺寸和11px圆角生成，大小变化时更新。
- GDK自己提交下一帧；窗口unrealize时销毁自有对象，显示closed信号在GDK后端dispose前释放借用资源。该顺序已核对 [GTK关闭实现](https://raw.githubusercontent.com/GNOME/gtk/4.22.0/gdk/gdkdisplay.c)。
- 不支持协议时无配置修改、无后台截图，继续使用较深的0.88-alpha底；支持时使用对比度验证过的0.82-alpha材质。
- 仅增加libwayland互操作所需的Rust依赖；没有安装系统包、改niri配置、替换已安装程序。

## 3. 不是“发了请求”就算视觉成功

[视觉测试](../crates/vellum-ui/src/background_blur_live_test.rs)创建生成的背景层和不透明背板，再显示普通GTK测试窗；只在经过包含关系和焦点验证的测试区域内抓取128×32像素，均不写盘。

| 同一条纹画面 | 平均行内亮暗范围 |
|---|---:|
| 尚未请求模糊 | 144.00 |
| 标准协议请求、GDK提交之后 | 3.81 |

测试窗口的透明度在前后保持相同，因而不是通过加深窗口底色掩盖条纹。细节对比减少约97%，确认由合成器实际处理。测试走生产挂载入口，关闭窗口正常；额外在所有GTK窗口/回调释放后关闭测试显示连接也通过。不能把它描述成“任意异常断线恢复已验证”。

模糊来源、强度和xray策略由合成器决定；niri默认xray通常只看壁纸。这与程序读取后方窗口像素不同。以前的WidgetPaintable/GSK导出只包含应用表面，不包含合成器背景后处理，不能拿这些PNG假装展示真实桌面模糊。

## 4. 复查命令

~~~sh
GSK_RENDERER=cairo VELLUM_LONGSHOT_TRACE=1 cargo test --locked --release --workspace recorder::live_tests::live_fast_scroll_capture_preserves_every_row -- --ignored --exact --nocapture --test-threads=1
cargo test --locked --release --workspace recorder::live_tests::live_reports_background_effect_protocols -- --ignored --exact --nocapture --test-threads=1
GSK_RENDERER=cairo cargo test --locked --release --workspace background_blur::live_tests::live_compositor_blur_reduces_background_detail -- --ignored --exact --nocapture --test-threads=1
VELLUM_UI_DEMO=1 target/release/vellum-ui panel
~~~

## 5. 仍未解决的独立条件

本机全局快捷键真实绑定仍失败。当前niri门户偏好为 default=gnome;gtk;，没有用户级门户覆盖；GNOME后端依赖的org.gnome.Shell在该会话没有所有者。模糊协议可用不代表GlobalShortcuts后端可用。旧快捷键、系统设置与安装版本仍保留，不以root键盘监听或偷改配置绕过这一限制。
