#!/usr/bin/env bash
# pngshot 一键安装脚本
#
# 用法：
#   curl -fsSL https://raw.githubusercontent.com/tjz123psh/-Screenshot-Tool/main/install.sh | bash
#
# 做的事：
#   1. 在 Arch Linux 上自动安装缺失的运行依赖（只在确实缺包时请求 sudo）
#   2. 把源码克隆/更新到 ~/.local/share/pngshot
#   3. 安装 pngshot / pngshotctl 启动器与应用菜单入口
#   4. 安装并启动 systemd 用户服务与系统托盘
#   5. 检查依赖与环境，列出仍缺失的项目
#   6. 提示 ~/.local/bin 是否在 PATH
#
# 重复运行是幂等的：已存在则 git pull 更新，再重装启动器。
set -euo pipefail

REPO_URL="${PNGSHOT_REPO_URL:-https://github.com/tjz123psh/-Screenshot-Tool.git}"
SRC_DIR="${PNGSHOT_ROOT:-$HOME/.local/share/pngshot}"
BIN_DIR="${PNGSHOT_BIN_DIR:-$HOME/.local/bin}"
LAUNCHER="$BIN_DIR/pngshot"
CTL_LAUNCHER="$BIN_DIR/pngshotctl"
SYSTEMD_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
APPLICATION_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
ICON_APP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps"
ICON_STATUS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/status"

info()  { printf '\033[1;34m::\033[0m %s\n' "$*"; }
ok()    { printf '\033[1;32m✓\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m!\033[0m %s\n' "$*"; }
die()   { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

# --- 0. 安装 Arch 运行依赖 ------------------------------------------------
REQUIRED_PACKAGES=(
    git python grim wl-clipboard libnotify tesseract
    tesseract-data-chi_sim tesseract-data-eng
    python-gobject python-cairo gtk4-layer-shell libayatana-appindicator
    python-pillow python-opencv python-numpy
)

if [[ "${PNGSHOT_SKIP_PACKAGES:-0}" != "1" ]]; then
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
            ok "运行依赖已安装"
        else
            ok "运行依赖已齐全"
        fi
    else
        warn "未检测到 pacman；将跳过自动装包，继续检查当前环境"
    fi
fi

# --- 前置工具 -------------------------------------------------------------
command -v git >/dev/null     || die "需要 git，请先安装：sudo pacman -S git"
command -v python3 >/dev/null || die "需要 python3，请先安装：sudo pacman -S python"

# --- 1. 克隆 / 更新源码 ---------------------------------------------------
if [[ -d "$SRC_DIR/.git" ]]; then
    info "更新已有源码：$SRC_DIR"
    git -C "$SRC_DIR" pull --ff-only || warn "git pull 失败，沿用现有版本"
else
    info "克隆源码到：$SRC_DIR"
    mkdir -p "$(dirname "$SRC_DIR")"
    git clone --depth 1 "$REPO_URL" "$SRC_DIR"
fi
ok "源码就绪：$SRC_DIR"

# --- 2. 安装启动器 --------------------------------------------------------
info "安装启动器：$LAUNCHER"
mkdir -p "$BIN_DIR"
cat > "$LAUNCHER" <<EOF
#!/usr/bin/env bash
# pngshot 启动器（由 install.sh 生成）。设置 PYTHONPATH 与 LD_PRELOAD 后运行模块。
set -euo pipefail
PNGSHOT_ROOT="\${PNGSHOT_ROOT:-$SRC_DIR}"
export PYTHONPATH="\$PNGSHOT_ROOT\${PYTHONPATH:+:\$PYTHONPATH}"

# gtk4-layer-shell 必须在 libwayland-client 之前加载；PyGObject 的链接顺序相反，
# 因此预加载官方推荐的 shim 修复 layer-shell 失效问题。
_LAYER_SHELL_LIB="/usr/lib/libgtk4-layer-shell.so"
if [[ "\${1:-}" != "tray" && -e "\$_LAYER_SHELL_LIB" ]]; then
    export LD_PRELOAD="\${_LAYER_SHELL_LIB}\${LD_PRELOAD:+:\$LD_PRELOAD}"
fi

exec python3 -m pngshot "\$@"
EOF
chmod +x "$LAUNCHER"
ln -sfn "pngshot" "$CTL_LAUNCHER"
ok "启动器已安装"

# --- 3. 后台服务与系统托盘 ------------------------------------------------
info "安装截图服务与系统托盘"
mkdir -p "$SYSTEMD_DIR" "$APPLICATION_DIR" "$ICON_APP_DIR" "$ICON_STATUS_DIR"
sed "s|@PNGSHOT_LAUNCHER@|$LAUNCHER|g" \
    "$SRC_DIR/contrib/pngshot.service" > "$SYSTEMD_DIR/pngshot.service"
sed "s|@PNGSHOT_LAUNCHER@|$LAUNCHER|g" \
    "$SRC_DIR/contrib/pngshot-tray.service" > "$SYSTEMD_DIR/pngshot-tray.service"
# 安装应用菜单启动器与应用/托盘图标。
sed "s|@PNGSHOT_LAUNCHER@|$LAUNCHER|g" \
    "$SRC_DIR/contrib/ai.pngshot.desktop" > "$APPLICATION_DIR/ai.pngshot.desktop"
install -m 0644 "$SRC_DIR/contrib/icons/ai.pngshot.svg" \
    "$ICON_APP_DIR/ai.pngshot.svg"
install -m 0644 "$SRC_DIR/contrib/icons/ai.pngshot-symbolic.svg" \
    "$ICON_STATUS_DIR/ai.pngshot-symbolic.svg"
install -m 0644 "$SRC_DIR/contrib/icons/ai.pngshot-recording-symbolic.svg" \
    "$ICON_STATUS_DIR/ai.pngshot-recording-symbolic.svg"
install -m 0644 "$SRC_DIR/contrib/icons/ai.pngshot-warning-symbolic.svg" \
    "$ICON_STATUS_DIR/ai.pngshot-warning-symbolic.svg"
# Remove the rejected full-size control-center entry from older installations.
rm -f "$APPLICATION_DIR/ai.pngshot.ControlCenter.desktop"
# Refresh user caches when the desktop provides the helpers.  Both commands
# are best-effort: the files themselves remain valid without a cache, but a
# running launcher may otherwise wait until the next login to notice them.
if command -v gtk-update-icon-cache >/dev/null; then
    gtk-update-icon-cache -f -t "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor" \
        >/dev/null 2>&1 || true
fi
if command -v update-desktop-database >/dev/null; then
    update-desktop-database "$APPLICATION_DIR" >/dev/null 2>&1 || true
fi

if command -v systemctl >/dev/null; then
    systemctl --user daemon-reload
    # `enable --now` does not replace an already-running daemon after an
    # upgrade. The CLI restart handshake also shuts down a previous directly
    # spawned instance before systemd starts the newly installed code.
    if systemctl --user enable pngshot.service pngshot-tray.service \
        && "$LAUNCHER" restart \
        && systemctl --user restart pngshot-tray.service; then
        ok "截图服务与系统托盘已启动，并将在登录后自动运行"
    else
        warn "截图服务暂未启动；快捷键调用时仍会自动拉起"
    fi
else
    warn "未找到 systemctl；快捷键调用时会按需启动服务"
fi

# --- 4. 依赖检查 ----------------------------------------------------------
info "检查系统依赖"

# 命令行工具 -> 所属 pacman 包
declare -A CMD_PKG=(
    [grim]=grim
    [wl-copy]=wl-clipboard
    [notify-send]=libnotify
    [tesseract]=tesseract
)
# Python 模块 -> 所属 pacman 包
declare -A PY_PKG=(
    [gi]=python-gobject
    [cairo]=python-cairo
    [PIL]=python-pillow
    [cv2]=python-opencv
    [numpy]=python-numpy
)

missing=()
for cmd in "${!CMD_PKG[@]}"; do
    command -v "$cmd" >/dev/null || missing+=("${CMD_PKG[$cmd]}")
done
for mod in "${!PY_PKG[@]}"; do
    python3 -c "import $mod" >/dev/null 2>&1 || missing+=("${PY_PKG[$mod]}")
done
# gtk4-layer-shell 是 .so，单独检查
[[ -e /usr/lib/libgtk4-layer-shell.so ]] || missing+=(gtk4-layer-shell)
python3 -c "import gi; gi.require_version('AyatanaAppIndicator3', '0.1')" \
    >/dev/null 2>&1 || missing+=(libayatana-appindicator)
# tesseract 中英语言包（命令存在时才细查）
if command -v tesseract >/dev/null; then
    langs="$(tesseract --list-langs 2>/dev/null || true)"
    grep -qx chi_sim <<<"$langs" || missing+=(tesseract-data-chi_sim)
    grep -qx eng     <<<"$langs" || missing+=(tesseract-data-eng)
fi

if ((${#missing[@]})); then
    # 去重
    mapfile -t missing < <(printf '%s\n' "${missing[@]}" | sort -u)
    warn "缺少以下依赖，请用 pacman 安装："
    printf '\n    sudo pacman -S %s\n\n' "${missing[*]}"
else
    ok "系统依赖齐全"
fi

# opencode 是翻译功能的可选依赖
command -v opencode >/dev/null || warn "未检测到 opencode（仅翻译功能需要，其它功能不受影响）"

# --- 5. PATH 检查 ---------------------------------------------------------
case ":$PATH:" in
    *":$BIN_DIR:"*) ok "$BIN_DIR 已在 PATH 中" ;;
    *) warn "$BIN_DIR 不在 PATH，请加入你的 shell 配置，例如：
    echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.bashrc" ;;
esac

echo
ok "安装完成。Pngshot 相机图标已加入系统托盘。"
info "状态检查：pngshotctl status；完整诊断：pngshotctl doctor"
info "niri 键位与窗口规则示例见：$SRC_DIR/contrib/niri-pngshot.kdl"
