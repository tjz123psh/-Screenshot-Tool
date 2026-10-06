#!/usr/bin/env bash
# One-click install of the newest verified Vellum release.
#
#   curl -fsSL https://github.com/tjz123psh/-Screenshot-Tool/releases/latest/download/install.sh | bash
#
# It downloads the release bundle, verifies its SHA256 and hands the extracted
# bundle to the version manager. It never builds anything, never needs root and
# never writes compositor configuration.
#
#   ... | bash -s -- --adopt-legacy     take over a verified older install
#   VELLUM_RELEASE_TAG=v0.2.0           install one specific release
#   VELLUM_RELEASE_REPO=owner/repo      another fork
#   VELLUM_RELEASE_BASE=<url>           another download base (also file://)
set -eo pipefail

REPO=${VELLUM_RELEASE_REPO:-tjz123psh/-Screenshot-Tool}
TAG=${VELLUM_RELEASE_TAG:-latest}
BASE=${VELLUM_RELEASE_BASE:-https://github.com/$REPO/releases}
URL="$BASE/$TAG/download"
ASSET=vellum-linux-x86_64.tar.gz

say() { printf '%s\n' "$*"; }
die() { printf 'vellum: %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -ne 0 ] || die '请用普通用户运行：程序安装在你的主目录，不需要也不要使用 sudo'

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 --connect-timeout 20 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        die '需要 curl 或 wget'
    fi
}

command -v tar >/dev/null 2>&1 || die '需要 tar'
command -v sha256sum >/dev/null 2>&1 || die '需要 sha256sum（coreutils）'

umask 077
TMP=$(mktemp -d "${TMPDIR:-/tmp}/vellum-install.XXXXXX") || die '无法创建临时目录'
cleanup() {
    # Exactly the directory created above; nothing else is ever removed.
    case "$TMP" in
        */vellum-install.*)
            [ -f "$TMP/.vellum-install" ] && rm -rf -- "$TMP"
            ;;
    esac
}
trap cleanup EXIT
: > "$TMP/.vellum-install"

say "下载 $REPO 的 $TAG 发行包"
fetch "$URL/$ASSET" "$TMP/$ASSET" \
    || die "下载失败：$URL/$ASSET（该版本可能还没有发行包）"
fetch "$URL/$ASSET.sha256" "$TMP/$ASSET.sha256" \
    || die '下载校验和失败，已拒绝安装未校验的包'
( cd "$TMP" && sha256sum -c "$ASSET.sha256" >/dev/null 2>&1 ) \
    || die 'SHA256 校验不通过，已拒绝安装'
say '校验和通过'

tar -xzf "$TMP/$ASSET" -C "$TMP" || die '解压失败'
BUNDLE=$(find "$TMP" -maxdepth 1 -mindepth 1 -type d -name 'vellum-*' | head -1)
[ -n "$BUNDLE" ] && [ -d "$BUNDLE" ] || die '发行包结构异常'
for member in bin/vellum bin/vellumctl bin/vellum-ui bin/vellum-tray install.sh manifest.json; do
    [ -e "$BUNDLE/$member" ] || die "发行包缺少 $member"
done

say "安装 $(basename "$BUNDLE")"
bash "$BUNDLE/install.sh" "$@"
status=$?
if [ "$status" -eq 0 ]; then
    say '完成。工作台：vellum panel　　状态：vellum release status'
fi
exit "$status"
