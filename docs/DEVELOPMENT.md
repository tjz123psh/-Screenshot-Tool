# 开发与发布

[返回首页](<../README.md>) · [文档导航](<README.md>) · [贡献约定](<../CONTRIBUTING.md>)

## 默认交付方式

本项目默认只安装正式发布的二进制包。本机不从源码构建、不为构建安装系统依赖；完整编译、严格 clippy、测试和依赖审计由 Arch Linux CI 执行。

1. 修改源码与对应测试，先做可运行的轻量检查。
2. 推送 main，由 [CI 工作流](<../.github/workflows/ci.yml>)执行完整检查。
3. 需要发版时才创建与 Cargo 版本对应的 `vX.Y.Z` 标签，并在同一标签上手动触发 [release 工作流](<../.github/workflows/release.yml>)。不要移动或覆盖已发布标签。
4. 用户通过发布的一键脚本安装；仅修改文档和测试维护代码时，不必重新安装程序。

```sh
curl -fsSL https://github.com/tjz123psh/-Screenshot-Tool/releases/latest/download/install.sh | bash
```

## 本地轻量检查

仅使用本机已有工具，不为检查安装构建依赖：

```sh
cargo fmt --check
PYTHONDONTWRITEBYTECODE=1 python3 tests/package-release.py
bash -n install.sh install-remote.sh install-release.sh
git diff --check
```

Rust 工具链未安装时，不必为了本地格式检查安装它，交给 CI 即可。图形测试按测试说明显式运行，不把默认忽略的用例说成已经验收。完整基准与真机限制见 [性能记录](<../PERFORMANCE.md>)；旧记录中的通过数量不是当前测试总数。

## 脚本与目录职责

| 入口 | 用途 |
| --- | --- |
| [install-release.sh](<../install-release.sh>) | 用户默认入口，作为 Release 中的 install.sh 发布，下载并校验二进制包 |
| [contrib/install-bundle.sh](<../contrib/install-bundle.sh>) | 发行包内部入口，委托版本管理器 |
| [tools/package_release.py](<../tools/package_release.py>) | CI 打包、清单校验与构建身份检查 |
| [tests](<../tests/>) | 安装、打包、发布故障矩阵 |
| [install.sh](<../install.sh>)、[install-remote.sh](<../install-remote.sh>) | 保留的源码开发兼容入口，不是本机或用户默认安装方式 |

发布清单与故障恢复细节见 [RELEASE_FORMAT](<RELEASE_FORMAT.md>)。

## 保留的维护参考

以下说明从原首页移入，供隔离开发环境和安装器维护使用；它不改变本项目“本机只安装发布包”的要求。命令中的相对路径均以仓库根或明确指定的发行包目录为准。

### 手动安装已校验的二进制包

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

### 开发者从源码安装

仅在你确实要改代码时才需要本节。它针对仓库源码根目录，和发行包内的小安装入口不同：

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

详情见[发布格式](<RELEASE_FORMAT.md>)与[连续执行计划](<PRODUCT_EXECUTION_PLAN.md>)。


