#!/usr/bin/env bash
# vellum installer.
#
# Usage (from a checkout):
#   ./install.sh
#
# What it does:
#   1. Installs missing Arch runtime/build packages (asks for sudo only when a
#      package is genuinely absent)
#   2. Builds the release binaries with cargo
#   3. Installs vellum / vellumctl / vellum-ui / vellum-tray into ~/.local/bin
#   4. Installs the desktop entry and the application/status icons
#   5. Tries to write the default shortcuts into the user's Niri keybinds.kdl
#   6. Installs and starts the systemd user service and the tray
#   7. Reports anything still missing and whether ~/.local/bin is on PATH
#
# Re-running is idempotent: cargo rebuilds only what changed and every install
# step overwrites in place.
#
# Unlike its Python predecessor this script does not clone anything.  It builds
# the tree it lives in, so there is no remote to drift from and no second copy
# of the source to keep in sync.
set -euo pipefail

SRC_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${VELLUM_BIN_DIR:-$HOME/.local/bin}"
LAUNCHER="$BIN_DIR/vellum"
TRAY_BIN="$BIN_DIR/vellum-tray"
SYSTEMD_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
APPLICATION_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
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
    rust pkgconf gtk4 gtk4-layer-shell
    grim wl-clipboard libnotify
    tesseract tesseract-data-chi_sim tesseract-data-eng
)

if [[ "${VELLUM_SKIP_PACKAGES:-0}" != "1" ]]; then
    if command -v pacman >/dev/null; then
        mapfile -t missing_system < <(pacman -T "${REQUIRED_PACKAGES[@]}" 2>/dev/null || true)
        if ((${#missing_system[@]})); then
            info "检测到缺失依赖，准备安装：${missing_system[*]}"
            if [[ $EUID -eq 0 ]]; then
                pacman -S --needed --noconfirm "${missing_system[@]}" \
                    || die "依赖安装失败"
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

# --- 1. Build -------------------------------------------------------------
info "构建 release 二进制（首次构建需要几分钟）"
cargo build --release --manifest-path "$SRC_DIR/Cargo.toml" \
    || die "构建失败"
for bin in "${BINARIES[@]}"; do
    [[ -x "$SRC_DIR/target/release/$bin" ]] || die "构建产物缺失：$bin"
done
ok "构建完成"

# --- 2. Install binaries --------------------------------------------------
info "安装二进制到：$BIN_DIR"
mkdir -p "$BIN_DIR"
for bin in "${BINARIES[@]}"; do
    # install(1) replaces the file rather than writing through it, so a running
    # daemon or tray keeps its own inode and is not corrupted mid-flight.
    install -m 0755 "$SRC_DIR/target/release/$bin" "$BIN_DIR/$bin"
done
ok "二进制已安装"

# --- 3. Niri shortcuts (degradable) ---------------------------------------
# Shortcuts live in the user's Niri config and must not be a hard dependency of
# the install: on a conflict, a missing config.kdl or a failed validation we
# only point at the example and carry on with the service and tray.
if [[ "${VELLUM_SKIP_SHORTCUTS:-0}" == "1" ]]; then
    info "已跳过 Niri 快捷键自动配置（VELLUM_SKIP_SHORTCUTS=1）"
elif [[ -f "${XDG_CONFIG_HOME:-$HOME/.config}/niri/config.kdl" ]]; then
    shortcut_output=""
    if shortcut_output=$("$LAUNCHER" shortcuts install 2>&1); then
        printf '%s\n' "$shortcut_output"
        ok "Niri 快捷键已自动配置或已存在"
    else
        shortcut_status=$?
        printf '%s\n' "$shortcut_output"
        warn "Niri 快捷键自动配置未完成（状态 $shortcut_status）；不影响 vellum 安装"
        warn "请参考：$SRC_DIR/contrib/niri-vellum.kdl"
    fi
else
    warn "未找到 Niri config.kdl，跳过快捷键自动配置"
    warn "安装后可参考：$SRC_DIR/contrib/niri-vellum.kdl"
fi

# --- 4. Service, tray, desktop entry, icons -------------------------------
info "安装截图服务与系统托盘"
mkdir -p "$SYSTEMD_DIR" "$APPLICATION_DIR" "$ICON_APP_DIR" "$ICON_STATUS_DIR"
sed "s|@VELLUM_LAUNCHER@|$LAUNCHER|g" \
    "$SRC_DIR/contrib/vellum.service" > "$SYSTEMD_DIR/vellum.service"
sed "s|@VELLUM_TRAY@|$TRAY_BIN|g" \
    "$SRC_DIR/contrib/vellum-tray.service" > "$SYSTEMD_DIR/vellum-tray.service"
sed "s|@VELLUM_LAUNCHER@|$LAUNCHER|g" \
    "$SRC_DIR/contrib/ai.vellum.desktop" > "$APPLICATION_DIR/ai.vellum.desktop"
install -m 0644 "$SRC_DIR/contrib/icons/ai.vellum.svg" \
    "$ICON_APP_DIR/ai.vellum.svg"
for icon in ai.vellum-symbolic ai.vellum-recording-symbolic ai.vellum-warning-symbolic; do
    install -m 0644 "$SRC_DIR/contrib/icons/$icon.svg" "$ICON_STATUS_DIR/$icon.svg"
done

# Cache refreshes are best-effort: the files are valid without them, but a
# running shell may otherwise not notice the new icon until the next login.
if command -v gtk-update-icon-cache >/dev/null; then
    gtk-update-icon-cache -f -t "$ICON_ROOT" >/dev/null 2>&1 || true
fi
if command -v update-desktop-database >/dev/null; then
    update-desktop-database "$APPLICATION_DIR" >/dev/null 2>&1 || true
fi

if command -v systemctl >/dev/null; then
    systemctl --user daemon-reload
    # `enable --now` will not replace an already-running daemon after an
    # upgrade.  `vellum restart` also shuts down an instance that a hotkey
    # spawned directly, before systemd starts the freshly installed code.
    if systemctl --user enable vellum.service vellum-tray.service \
        && "$LAUNCHER" restart \
        && systemctl --user restart vellum-tray.service; then
        ok "截图服务与系统托盘已启动，并将在登录后自动运行"
    else
        warn "截图服务暂未启动；快捷键调用时仍会自动拉起"
    fi
else
    warn "未找到 systemctl；快捷键调用时会按需启动服务"
fi

# --- 5. Environment check -------------------------------------------------
# `vellum doctor` is the single source of truth for what vellum needs; the
# installer just runs it instead of maintaining a second, drifting checklist.
info "检查运行环境"
if "$LAUNCHER" doctor; then
    ok "环境检查通过"
else
    warn "环境检查发现问题，详见上方 ✗ 条目"
fi

# --- 6. PATH check --------------------------------------------------------
case ":$PATH:" in
    *":$BIN_DIR:"*) ok "$BIN_DIR 已在 PATH 中" ;;
    *) warn "$BIN_DIR 不在 PATH，请加入你的 shell 配置，例如：
    echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.bashrc" ;;
esac

echo
ok "安装完成，vellum 图标已加入系统托盘。"
info "状态检查：vellum status；完整诊断：vellum doctor"
info "niri 键位与窗口规则示例见：$SRC_DIR/contrib/niri-vellum.kdl"
