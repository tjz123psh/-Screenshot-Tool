#!/usr/bin/env bash
# Explicit developer source channel. Ordinary users should install a verified
# binary bundle; this script never silently fetches a moving main branch.
set -euo pipefail
REF="${VELLUM_SOURCE_REF:-}"
REPO="${VELLUM_SOURCE_REPO:-tjz123psh/-Screenshot-Tool}"
die() { printf '%s\n' "$*" >&2; exit 1; }
[[ "$REF" =~ ^[0-9a-f]{40}$ ]] || die '远程源码安装需要 VELLUM_SOURCE_REF=完整40位提交SHA；普通用户请使用版本化发行包，不会自动拉取main。'
[[ "$REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ && "$REPO" != ../* && "$REPO" != */.. ]] || die '源码仓库名称无效'
command -v git >/dev/null || die '固定提交源码安装需要git；二进制发行包不需要此步骤'
umask 077
TMP_BASE="$(cd -- "${TMPDIR:-/tmp}" && pwd -P)"
TMP="$(mktemp -d "$TMP_BASE/vellum-fixed-source.XXXXXX")"
cleanup() {
    if [[ -n "$TMP" && ! -L "$TMP" && -f "$TMP/.vellum-fixed-source" ]]; then
        case "$TMP" in "$TMP_BASE"/vellum-fixed-source.*) rm -rf -- "$TMP" ;; esac
    fi
}
trap cleanup EXIT
printf '%s\n' 'private fixed-source checkout' > "$TMP/.vellum-fixed-source"
safe_git() {
    env -u GIT_CONFIG_COUNT -u GIT_CONFIG_PARAMETERS GIT_CONFIG_NOSYSTEM=1 \
        GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
        git -c core.hooksPath=/dev/null -c credential.helper= \
        -c protocol.file.allow=never -c submodule.recurse=false "$@"
}
printf '获取固定提交 %s（开发源码通道）\n' "$REF"
safe_git init --quiet --template= "$TMP/src" > "$TMP/git.log" 2>&1 || die '无法创建私有源码目录，安装版未修改'
safe_git -C "$TMP/src" remote add origin "https://github.com/$REPO.git" >> "$TMP/git.log" 2>&1 || die '无法设置源码来源，安装版未修改'
safe_git -C "$TMP/src" fetch --quiet --depth=1 --no-tags origin "$REF" >> "$TMP/git.log" 2>&1 || die '无法获取指定提交；请检查网络、仓库权限与提交SHA。安装版未修改'
actual="$(safe_git -C "$TMP/src" rev-parse --verify FETCH_HEAD 2>> "$TMP/git.log")" || die '无法验证源码提交'
[[ "$actual" == "$REF" ]] || die '实际源码提交不匹配，已拒绝安装'
safe_git -C "$TMP/src" checkout --quiet --detach FETCH_HEAD >> "$TMP/git.log" 2>&1 || die '无法检出已验证提交'
[[ -f "$TMP/src/install.sh" && -f "$TMP/src/tools/package_release.py" && -f "$TMP/src/crates/vellum-cli/src/release.rs" ]] || die '此提交尚不支持版本化安装；请使用新版本发行包，未运行旧式覆盖安装器'
(
    cd -- "$TMP/src"
    VELLUM_REMOTE_INSTALL=1 bash install.sh
)
