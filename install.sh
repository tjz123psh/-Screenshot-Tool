#!/usr/bin/env bash
# Developer/source install: check build dependencies, build all four programs,
# validate their shared identity, prepare a complete version bundle, then delegate
# live mutation to `vellum release install`. No compositor configuration writes.
# Source/build trees stay available for continued development; only the private
# temporary bundle is cleaned. Published binary bundles use their own install.sh.
# VELLUM_ADOPT_LEGACY=1 explicitly permits migration of a verified old installation.
set -euo pipefail

SRC_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${VELLUM_BIN_DIR:-$HOME/.local/bin}"
LAUNCHER="$BIN_DIR/vellum"
TRAY_BIN="$BIN_DIR/vellum-tray"
SYSTEMD_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
APPLICATION_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
DBUS_SERVICE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/dbus-1/services"
AUTOSTART_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
ICON_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor"
ICON_APP_DIR="$ICON_ROOT/scalable/apps"
ICON_STATUS_DIR="$ICON_ROOT/scalable/status"

# The binaries vellum ships.  `vellum` and `vellumctl` stay free of GTK so the
# hotkey path does not pay for a toolkit it never draws with; `vellum-ui` is the
# only GTK process and `vellum-tray` talks D-Bus only.
BINARIES=(vellum vellumctl vellum-ui vellum-tray)

info()  { printf '\033[1;34m::\033[0m %s\n' "$*"; }
ok()    { printf '\033[1;32m✓\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m!\033[0m %s\n' "$*"; }
die()   { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

# --- 0. Arch packages -----------------------------------------------------
# Build-time: rust, pkgconf and the GTK development files.  Runtime: the
# external tools vellum shells out to.  tesseract is used as a subprocess
# rather than linked, so no leptonica development package is needed.
REQUIRED_PACKAGES=(
    rust pkgconf python gtk4 gtk4-layer-shell xdg-desktop-portal
    grim wl-clipboard libnotify
    tesseract tesseract-data-chi_sim tesseract-data-eng
)

if [[ "${VELLUM_SKIP_PACKAGES:-0}" != "1" ]]; then
    if command -v pacman >/dev/null; then
        # `pacman -T` uses 127 specifically for unsatisfied dependencies.
        # Any other non-zero status means the query itself failed (database,
        # permissions, corrupt config) and must not be reported as "all ready".
        package_query_stderr_file=$(mktemp "${TMPDIR:-/tmp}/vellum-pacman-query.XXXXXX") \
            || die "无法创建 pacman 依赖查询临时文件"
        if package_query=$(pacman -T "${REQUIRED_PACKAGES[@]}" 2> "$package_query_stderr_file"); then
            package_query_status=0
        else
            package_query_status=$?
        fi
        package_query_stderr=$(< "$package_query_stderr_file")
        rm -f -- "$package_query_stderr_file"

        case "$package_query_status" in
            0) missing_system=() ;;
            127)
                if [[ -z "$package_query" ]]; then
                    die "依赖查询失败：pacman -T 未返回缺失包${package_query_stderr:+（$package_query_stderr）}"
                fi
                mapfile -t missing_system <<< "$package_query"
                ;;
            *)
                package_query_detail="${package_query_stderr:-$package_query}"
                die "依赖查询失败（pacman -T 状态 $package_query_status）${package_query_detail:+：$package_query_detail}"
                ;;
        esac
        if [[ -n "$package_query_stderr" ]]; then
            warn "pacman -T：$package_query_stderr"
        fi
        if ((${#missing_system[@]})); then
            info "检测到缺失依赖，准备安装：${missing_system[*]}"
            if [[ $EUID -eq 0 ]]; then
                pacman -S --needed --noconfirm "${missing_system[@]}" \
                    || die "依赖安装失败"
            elif [[ -x "$HOME/scripts/desktop/gsudo" ]]; then
                "$HOME/scripts/desktop/gsudo" pacman -S --needed --noconfirm "${missing_system[@]}" \
                    || die "依赖安装未完成或已取消；不会自动重试"
            elif command -v sudo >/dev/null; then
                sudo pacman -S --needed --noconfirm "${missing_system[@]}" \
                    || die "依赖安装失败"
            else
                die "缺少 sudo，无法自动安装依赖：${missing_system[*]}"
            fi
            ok "依赖已安装"
        else
            ok "依赖已齐全"
        fi
    else
        warn "未检测到 pacman；将跳过自动装包，继续检查当前环境"
    fi
fi

command -v cargo >/dev/null || die "需要 cargo，请先安装：sudo pacman -S rust"

# --- Build and prepare a complete candidate -------------------------------
PYTHON="${VELLUM_PACKAGE_PYTHON:-python3}"
command -v "$PYTHON" >/dev/null || die "源码打包需要 python3；二进制发行包不需要此步骤"
info "构建 release 二进制"
cargo build --locked --release --manifest-path "$SRC_DIR/Cargo.toml" || die "构建失败"
for bin in "${BINARIES[@]}"; do
    [[ -x "$SRC_DIR/target/release/$bin" ]] || die "构建产物缺失：$bin"
done

umask 077
TMP_BASE="$(cd -- "${TMPDIR:-/tmp}" && pwd -P)"
BUNDLE_TMP="$(mktemp -d "$TMP_BASE/vellum-source-bundle.XXXXXX")" || die "无法创建私有打包暂存目录"
cleanup_bundle() {
    # This exact directory was exclusively created above. No user files, source
    # tree, build cache, installed release, settings or recovery images are here.
    if [[ -n "$BUNDLE_TMP" && ! -L "$BUNDLE_TMP" && -f "$BUNDLE_TMP/.vellum-source-staging" ]]; then
        case "$BUNDLE_TMP" in
            "$TMP_BASE"/vellum-source-bundle.*) rm -rf -- "$BUNDLE_TMP" ;;
            *) warn "暂存目录校验失败，已保留而未删除" ;;
        esac
    fi
}
trap cleanup_bundle EXIT
printf '%s\n' "vellum-source-staging" > "$BUNDLE_TMP/.vellum-source-staging"
info "校验四个程序的构建身份并生成完整版本包"
"$PYTHON" "$SRC_DIR/tools/package_release.py" --root "$SRC_DIR" \
    --bin-dir "$SRC_DIR/target/release" --output "$BUNDLE_TMP/bundle" \
    || die "打包或构建身份校验失败，已安装版本未修改"

# --- Transactional user-level deployment ---------------------------------
# Legacy desktop keybindings are a separate explicit action, never an install side effect.
info "安装不修改 niri/Hyprland 快捷键；保留用户设置、截图与恢复资料"
manager=(release install --bundle "$BUNDLE_TMP/bundle" --bin-dir "$BIN_DIR" \
    --config-dir "${XDG_CONFIG_HOME:-$HOME/.config}" --data-dir "${XDG_DATA_HOME:-$HOME/.local/share}")
if [[ -n "${VELLUM_INSTALL_ROOT:-}" ]]; then manager+=(--root "$VELLUM_INSTALL_ROOT"); fi
if [[ "${VELLUM_ADOPT_LEGACY:-0}" == "1" ]]; then manager+=(--adopt-legacy); fi
if [[ "${VELLUM_NO_ACTIVATE:-0}" == "1" ]]; then manager+=(--no-activate); fi
"$SRC_DIR/target/release/vellum" "${manager[@]}" || die "版本管理器未完成安装；请以上方恢复/回退状态为准"

# The source channel is for development. Deleting target here could also erase
# a caller's custom installation root and defeats incremental development.
if [[ "${VELLUM_REMOTE_INSTALL:-0}" == "1" ]]; then
    info "远程临时源码将由外层清理；安装版本由用户级版本管理器持有"
else
    info "源码与构建目录已保留（兼容 VELLUM_SKIP_CLEANUP=1）；临时打包目录会清理"
fi
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) warn "$BIN_DIR 不在 PATH 中；桌面入口仍可使用，终端调用请使用完整路径" ;;
esac
ok "安装处理完成；实际激活状态以上方版本管理器报告为准"
