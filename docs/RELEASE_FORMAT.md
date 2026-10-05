# 第三批发布接口（实现约定）

以下是团队共享的v1格式，实施中由Lead协调变更。

## 构建信息

四个二进制统一支持 `--build-info-json`，该分支必须在GTK/DBus/服务/用户配置初始化前返回。JSON字段：format="vellum-build-info-v1"、version、build_id、source_commit、source_dirty、source_digest、target、rustc、profile、config_schema=1、ipc_schema=1。source_dirty为boolean，来源无法判断时为字符串"unknown"（Rust Option<bool>的明确序列化）；未知来源不能冒充clean，dirty本地构建不能冒充干净tag。build_id只允许安全单路径组件，最长96字节；包含固定版本及构建输入摘要，不包含用户目录/端点/凭据。

## 发行目录

- manifest.json（format="vellum-release-v1"，release_id=build.build_id，version=build.version，build为上面的完整对象）
- bin/vellum、bin/vellumctl、bin/vellum-ui、bin/vellum-tray
- resources/applications/*.desktop
- resources/autostart/ai.vellum-shortcuts.desktop
- resources/dbus-1/services/ai.vellum.Shortcuts.service
- resources/systemd/user/vellum*.service
- resources/icons/hicolor/scalable/apps/ai.vellum.svg
- resources/icons/hicolor/scalable/status/ai.vellum-symbolic.svg、ai.vellum-recording-symbolic.svg、ai.vellum-warning-symbolic.svg
- LICENSE、README.md、install.sh（包内小入口）

manifest.files是数组，每项path（相对且无..、无符号链接）、sha256（64位小写hex）、size、executable。manifest本身不列入files；所有其它成员必须列入，不能有未知/遗漏文件。资源文件保留@VELLUM_LAUNCHER@/@VELLUM_TRAY@模板，由安装器生成每版本的generated资源并另记哈希，不修改已提交版本。

## 管理入口

CLI `vellum release <install|status|rollback|repair|uninstall>`，由release模块定义完整clap子命令。install必需--bundle目录；支持--root/--bin-dir/--config-dir/--data-dir或等价共同路径参数以及--json。默认用户级目录。旧式安装接管必须显式--adopt-legacy，先备份/校验；没有清单时不得静默覆盖未知文件。uninstall必须显式确认参数，只删除清单且仍归属本程序的文件，默认保留全部设置/截图/恢复数据。

安装根默认 $HOME/.local/lib/vellum，可显式配置；同一锁覆盖安装/回退/修复/卸载。releases/<id>是完整、不可就地修改的版本，current是原子切换的相对链接。每个完整版本包含installed.json，format="vellum-installed-v1"、build_id、release_id及generated文件摘要；generated子树与resources去掉前缀后的结构相同。真实状态至少区分ready、installed-pending-activation、rolled-back、interrupted/needs-repair；不能服务未启动仍说托盘就绪。保留上一版本，不自动清理在用版本。

## 保证与边界

配置schema首期不迁移；任何安装/回退均不覆盖用户设置快照，不改niri/Hyprland配置。IPC managed版本请求须与服务匹配，状态/恢复管理仍可用；源目录开发运行兼容须明确，不悄悄使用另一版服务。包内版本一致性由构建信息和文件哈希共同检查；校验和只验证一致性/损坏，不独立证明发布者身份。官方工作流可提供构建来源证明，本机dirty包只作为开发包。
