#!/usr/bin/env bash
# An unreachable systemd user instance (sudo without XDG_RUNTIME_DIR, a
# container, no session bus) must degrade instead of aborting the install.
#
# By that point the installer has already replaced the binaries and written the
# unit files, so dying on systemctl --user daemon-reload leaves a half installed
# tree, skips the environment check and skips the build-tree cleanup — while
# install-remote.sh reports "installation failed".
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
TMP=$(mktemp -d "${TMPDIR:-/tmp}/vellum-install-systemd.XXXXXX")
trap 'rm -rf -- "$TMP"' EXIT

# A throwaway copy of the installer and its payload: the real checkout must not
# gain fake binaries under target/release.
SRC="$TMP/src"
mkdir -p "$SRC/contrib" "$TMP/bin" "$TMP/home"
cp "$ROOT/install.sh" "$SRC/install.sh"
cp -R "$ROOT/contrib/." "$SRC/contrib/"

cat > "$TMP/bin/cargo" <<'CARGO'
#!/usr/bin/env bash
# Stand in for the release build: produce the four binaries the installer
# checks for, without compiling anything.
mkdir -p "${VELLUM_TEST_SRC:?}/target/release"
for bin in vellum vellumctl vellum-ui vellum-tray; do
    printf '#!/usr/bin/env bash\nexit 0\n' > "$VELLUM_TEST_SRC/target/release/$bin"
    chmod 0755 "$VELLUM_TEST_SRC/target/release/$bin"
done
CARGO

cat > "$TMP/bin/systemctl" <<'SYSTEMCTL'
#!/usr/bin/env bash
echo 'Failed to connect to bus: No such file or directory' >&2
exit 1
SYSTEMCTL

chmod 0755 "$TMP/bin/cargo" "$TMP/bin/systemctl"

set +e
PATH="$TMP/bin:/usr/bin:/bin" \
HOME="$TMP/home" \
XDG_CONFIG_HOME="$TMP/home/.config" \
XDG_DATA_HOME="$TMP/home/.local/share" \
VELLUM_BIN_DIR="$TMP/home/.local/bin" \
VELLUM_SKIP_PACKAGES=1 \
VELLUM_SKIP_SHORTCUTS=1 \
VELLUM_TEST_SRC="$SRC" \
    bash "$SRC/install.sh" > "$TMP/stdout" 2> "$TMP/stderr"
status=$?
set -e

if ((status != 0)); then
    echo "installer aborted with an unreachable systemd user instance (status $status)" >&2
    tail -5 "$TMP/stderr" >&2
    exit 1
fi
# warn() prints to stdout, so the degradation notice lands there.
if ! grep -q '服务单元刷新' "$TMP/stdout"; then
    echo 'installer did not report the skipped daemon-reload' >&2
    exit 1
fi
if [[ ! -x "$TMP/home/.local/bin/vellum" ]]; then
    echo 'installer did not install the binaries' >&2
    exit 1
fi
if [[ -e "$SRC/target" ]]; then
    echo 'installer skipped the build-tree cleanup after the systemd step degraded' >&2
    exit 1
fi
if ! grep -q '安装完成' "$TMP/stdout"; then
    echo 'installer never reached its completion message' >&2
    exit 1
fi

echo 'installer survived an unreachable systemd user instance'
