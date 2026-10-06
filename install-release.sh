#!/usr/bin/env bash
# One-click install of the newest verified Vellum release.
#
#   curl -fsSL https://github.com/tjz123psh/-Screenshot-Tool/releases/latest/download/install.sh | bash
#
# It downloads the release bundle, verifies its SHA256, makes sure the runtime
# libraries the packaged programs link against are present, and hands the
# extracted bundle to the version manager. It never builds anything and never
# writes compositor configuration.
#
#   ... | bash -s -- --adopt-legacy     take over a verified older install
#   ... | bash -s -- --no-deps          do not touch system packages
#   VELLUM_RELEASE_TAG=v0.2.1           install one specific release
#   VELLUM_RELEASE_REPO=owner/repo      another fork
#   VELLUM_RELEASE_BASE=<url>           another download base (also file://)
set -eo pipefail

REPO=${VELLUM_RELEASE_REPO:-tjz123psh/-Screenshot-Tool}
TAG=${VELLUM_RELEASE_TAG:-latest}
BASE=${VELLUM_RELEASE_BASE:-https://github.com/$REPO/releases}
URL="$BASE/$TAG/download"
ASSET=vellum-linux-x86_64.tar.gz
SKIP_DEPS=${VELLUM_SKIP_DEPS:-0}

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

# The loader decides what is missing; this only names the package that provides
# each library on Arch. Anything unknown is reported by library name instead of
# guessing a package.
package_for_library() {
    case "$1" in
        libgtk4-layer-shell.so*) printf 'gtk4-layer-shell' ;;
        libgtk-4.so*|libgdk-4.so*|libgdk_pixbuf-2.0.so*) printf 'gtk4' ;;
        libnotify.so*) printf 'libnotify' ;;
        *) printf '' ;;
    esac
}

collect_missing_libraries() {
    local program
    for program in vellum vellumctl vellum-ui vellum-tray; do
        ldd "$BUNDLE/bin/$program" 2>/dev/null | awk '/not found/ {print $1}'
    done | sort -u
}

install_packages() {
    local -a installer=(pacman -S --needed --noconfirm)
    if [ "$(id -u)" -ne 0 ]; then
        command -v sudo >/dev/null 2>&1 || return 1
        installer=(sudo "${installer[@]}")
    fi
    printf '执行：%s %s\n' "${installer[*]}" "$*"
    "${installer[@]}" "$@"
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

# Everything that is not this script's own flag belongs to the version manager.
PASS_THROUGH=()
for argument in "$@"; do
    case "$argument" in
        --no-deps) SKIP_DEPS=1 ;;
        *) PASS_THROUGH+=("$argument") ;;
    esac
done

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

# A program that cannot even start would otherwise fail later with a vague
# message, so the runtime libraries are resolved before anything is installed.
missing=$(collect_missing_libraries)
if [ -n "$missing" ]; then
    packages=()
    unknown=()
    while IFS= read -r library; do
        [ -n "$library" ] || continue
        package=$(package_for_library "$library")
        if [ -n "$package" ]; then
            packages+=("$package")
        else
            unknown+=("$library")
        fi
    done <<< "$missing"
    if [ "${#packages[@]}" -gt 0 ]; then
        mapfile -t packages < <(printf '%s\n' "${packages[@]}" | sort -u)
        say "缺少运行库，需要这些软件包：${packages[*]}"
    fi
    if [ "${#unknown[@]}" -gt 0 ]; then
        say "另有无法自动匹配包的库：${unknown[*]}"
    fi
    if [ "$SKIP_DEPS" = "1" ]; then
        say '（--no-deps：跳过系统依赖，若安装失败请自行补齐上面的软件包）'
    elif [ "${#packages[@]}" -eq 0 ]; then
        die '无法自动确定提供这些库的软件包；请用系统包管理器安装后重试'
    elif command -v pacman >/dev/null 2>&1; then
        install_packages "${packages[@]}" \
            || die "安装运行依赖失败或被取消；请手动执行：sudo pacman -S --needed ${packages[*]}"
    else
        die "请用你的发行版包管理器安装提供以下库的软件包：${unknown[*]:-${packages[*]}}"
    fi
    remaining=$(collect_missing_libraries)
    [ -z "$remaining" ] || die "仍有运行库缺失：$remaining；未开始安装"
fi

# These only limit features; they never block an install.
optional_missing=()
for tool in grim wl-copy notify-send tesseract; do
    command -v "$tool" >/dev/null 2>&1 || optional_missing+=("$tool")
done
if [ "${#optional_missing[@]}" -gt 0 ]; then
    say "提示：未安装 ${optional_missing[*]}，相关功能（截图/复制/通知/本地识别）会受限"
fi

say "安装 $(basename "$BUNDLE")"
bash "$BUNDLE/install.sh" "${PASS_THROUGH[@]}"
status=$?
if [ "$status" -eq 0 ]; then
    say '完成。工作台：vellum panel　　状态：vellum release status'
fi
exit "$status"
