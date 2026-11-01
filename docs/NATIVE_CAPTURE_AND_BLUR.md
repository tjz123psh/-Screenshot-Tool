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

GTK 4.22+ 本身就用这条协议实现 CSS `backdrop-filter`，而协议规定一个 wl_surface 只能有一个背景效果对象：客户端再创建第二个，niri 会回 `wl_surface already has a background effect object attached` 并断开整个连接（表现就是设置窗口一闪就关，日志里是 `Gdk-Message: Error 71 (Protocol error)`）。

所以应用不再自己创建效果对象，[模糊模块](../crates/vellum-ui/src/background_blur.rs)只做能力探测：

- 只用于设置窗口；录制面板、图片内容和文字结果不因玻璃效果被透明化。
- 借用GTK现有Wayland连接（guest backend，不拥有连接），绑一次 manager 读 capabilities，**不创建任何表面对象**，也不覆盖GDK监听器、不替GTK读socket、不手动提交或销毁GTK的wl_surface。
- 确认 Blur 可用后给窗口加 `vellum-compositor-blur` 类；[theme.css](../crates/vellum-ui/src/theme.css) 在这个类上声明 `backdrop-filter: blur(24px)`，由GTK向合成器请求模糊，圆角与区域也归GTK处理。
- 不支持协议、或GTK低于4.22时保持 0.88-alpha 深底；不改配置、不截屏、不安装系统包。

## 3. 怎么算“模糊真的生效”

- 协议层（可复现，见第4节）：生产设置窗口在 `WAYLAND_DEBUG=1` 下应当只有 **1 次** `ext_background_effect_manager_v1.get_background_effect`、**1 次** `set_blur_region`、**0 条**协议错误；出现第二条 `get_background_effect` 就是又回到了“窗口瞬间消失”的老 bug。
- 视觉层：模糊半径与xray策略由合成器决定，GTK只声明“这一块要模糊”。GSK/WidgetPaintable 导出的窗口PNG不包含合成器后处理，不能拿它当模糊截图。
- 早先那一版“应用自建效果对象”曾用条纹窗口量化到 144 → 3.81；该实现已移除，且当前 niri 26.04 的平铺布局不会把测试背板合成到探针下面（采样范围 0.0），所以那组数字不再作为当前实现的证据，也不再保留对应的像素测试。

## 4. 复查命令

~~~sh
# 设置窗口的协议轨迹：期望 1 次 get_background_effect、1 次 set_blur_region、0 条错误
WAYLAND_DEBUG=1 vellum-ui panel > /tmp/panel-debug.log 2>&1 &
sleep 4; kill %1
grep -c get_background_effect /tmp/panel-debug.log   # 1
grep -c set_blur_region /tmp/panel-debug.log         # 1
grep -c 'wl_display.*error' /tmp/panel-debug.log     # 0

cargo test --locked --release --workspace recorder::live_tests::live_fast_scroll_capture_preserves_every_row -- --ignored --exact --nocapture --test-threads=1
cargo test --locked --release --workspace recorder::live_tests::live_reports_background_effect_protocols -- --ignored --exact --nocapture --test-threads=1
~~~

## 5. 仍未解决的独立条件

本机全局快捷键真实绑定仍失败。当前niri门户偏好为 default=gnome;gtk;，没有用户级门户覆盖；GNOME后端依赖的org.gnome.Shell在该会话没有所有者。模糊协议可用不代表GlobalShortcuts后端可用。旧快捷键、系统设置与安装版本仍保留，不以root键盘监听或偷改配置绕过这一限制。
