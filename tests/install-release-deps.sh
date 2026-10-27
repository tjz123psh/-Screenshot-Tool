#!/usr/bin/env bash
# The published installer must install the programs vellum shells out to
# (OCR, clipboard, notifications, wlroots capture), not just the linked GTK
# libraries. This drives install-release.sh against a local bundle, a minimal
# PATH and a fake package manager: no network, no root, no real packages.
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
TMP=$(mktemp -d "${TMPDIR:-/tmp}/vellum-release-deps.XXXXXX")
trap 'rm -rf -- "$TMP"' EXIT
mkdir -p "$TMP/bin" "$TMP/sys" "$TMP/home" "$TMP/pack/vellum-0.0.0-test/bin" "$TMP/dist/latest/download"

# The installer is driven with a PATH that holds only the utilities it needs,
# so a real tesseract or grim on the build machine cannot mask a test case.
for utility in bash sh id env curl mktemp rm tar gzip sha256sum find sort awk grep sed cat basename dirname ln wc tr head ldd; do
    target=$(command -v "$utility") || { echo "missing test utility: $utility" >&2; exit 1; }
    ln -s "$target" "$TMP/sys/$utility"
done

# Real ELF binaries: the installer resolves ldd output, and a script would
# abort the run before the package logic is reached.
for program in vellum vellumctl vellum-ui vellum-tray; do
    cp /usr/bin/true "$TMP/pack/vellum-0.0.0-test/bin/$program"
    chmod 0755 "$TMP/pack/vellum-0.0.0-test/bin/$program"
done
printf '{}\n' > "$TMP/pack/vellum-0.0.0-test/manifest.json"
cat > "$TMP/pack/vellum-0.0.0-test/install.sh" <<'BUNDLE'
#!/usr/bin/env bash
set -euo pipefail
printf 'installed' > "${VELLUM_TEST_INSTALL_MARKER:?}"
BUNDLE

ASSET=vellum-linux-x86_64.tar.gz
tar -czf "$TMP/dist/latest/download/$ASSET" -C "$TMP/pack" vellum-0.0.0-test
( cd "$TMP/dist/latest/download" && sha256sum "$ASSET" > "$ASSET.sha256" )

cat > "$TMP/bin/pacman" <<'PACMAN'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "${VELLUM_TEST_PACMAN_LOG:?}"
if [[ -n "${VELLUM_TEST_INSTALL_FAIL:-}" ]]; then
    echo 'install cancelled by user' >&2
    exit 1
fi
exit 0
PACMAN
# CI runs as root inside the Arch container; the installer refuses root on
# purpose, so a fake id makes it take the ordinary-user path.
cat > "$TMP/bin/id" <<'ID'
#!/usr/bin/env bash
if [[ "${1:-}" == "-u" ]]; then
    printf '1000\n'
    exit 0
fi
exec /usr/bin/id "$@"
ID
cat > "$TMP/bin/sudo" <<'SUDO'
#!/usr/bin/env bash
exec "$@"
SUDO
chmod 0755 "$TMP/bin/pacman" "$TMP/bin/sudo" "$TMP/bin/id"

run_installer() {
    PATH="$TMP/bin:$TMP/sys" HOME="$TMP/home" TMPDIR="$TMP" \
    VELLUM_RELEASE_BASE="file://$TMP/dist" \
    VELLUM_TEST_INSTALL_MARKER="$TMP/installed" \
    VELLUM_TEST_PACMAN_LOG="$TMP/pacman.log" \
        "$TMP/sys/bash" "$ROOT/install-release.sh" "$@"
}

# 1. Missing programs are installed through the package manager.
rm -f "$TMP/installed" "$TMP/pacman.log"
run_installer > "$TMP/out" 2> "$TMP/err"
grep -q '缺少系统依赖' "$TMP/out" || { echo 'installer did not report the missing packages' >&2; exit 1; }
grep -q 'tesseract-data-chi_sim' "$TMP/out" || { echo 'installer did not name the OCR language package' >&2; exit 1; }
grep -q -- '-S --needed --noconfirm' "$TMP/pacman.log" \
    || { echo 'installer did not call pacman -S' >&2; exit 1; }
for package in wl-clipboard libnotify tesseract tesseract-data-eng tesseract-data-chi_sim grim; do
    grep -q -- "$package" "$TMP/pacman.log" \
        || { echo "installer did not install $package" >&2; exit 1; }
done
[[ $(< "$TMP/installed") == installed ]] || { echo 'bundle was not installed after dependencies' >&2; exit 1; }

# 2. --no-deps must not touch the package manager at all.
rm -f "$TMP/installed" "$TMP/pacman.log"
run_installer --no-deps > "$TMP/out" 2> "$TMP/err"
[[ ! -e "$TMP/pacman.log" ]] || { echo '--no-deps still ran pacman' >&2; exit 1; }
[[ $(< "$TMP/installed") == installed ]] || { echo '--no-deps skipped the install' >&2; exit 1; }

# 3. A refused install stops before the bundle is deployed.
rm -f "$TMP/installed" "$TMP/pacman.log"
if VELLUM_TEST_INSTALL_FAIL=1 run_installer > "$TMP/out" 2> "$TMP/err"; then
    echo 'installer ignored a failed dependency install' >&2; exit 1
fi
[[ ! -e "$TMP/installed" ]] || { echo 'installer deployed the bundle after a failed dependency install' >&2; exit 1; }
grep -q '安装系统依赖失败或被取消' "$TMP/err" || { echo 'installer did not explain the failed dependency install' >&2; exit 1; }

# 4. Satisfied tools are never reinstalled, including the language data.
rm -f "$TMP/installed" "$TMP/pacman.log"
for tool in wl-copy wl-paste notify-send grim; do
    printf '#!/usr/bin/env bash\nexit 0\n' > "$TMP/bin/$tool"
done
cat > "$TMP/bin/tesseract" <<'TESSERACT'
#!/usr/bin/env bash
printf 'List of available languages in /usr/share/tesseract-ocr/5/tessdata/:\nchi_sim\neng\n'
TESSERACT
chmod 0755 "$TMP/bin/wl-copy" "$TMP/bin/wl-paste" "$TMP/bin/notify-send" "$TMP/bin/grim" "$TMP/bin/tesseract"
run_installer > "$TMP/out" 2> "$TMP/err"
[[ ! -e "$TMP/pacman.log" ]] || { echo 'installer reinstalled satisfied packages' >&2; exit 1; }
[[ $(< "$TMP/installed") == installed ]] || { echo 'bundle was not installed' >&2; exit 1; }

echo 'release installer installs OCR/clipboard/notification/capture tools and honours --no-deps'