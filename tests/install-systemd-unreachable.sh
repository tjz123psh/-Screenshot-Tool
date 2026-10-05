#!/usr/bin/env bash
# Source-wrapper contract: preserve a manager's pending-activation result and
# propagate failure. Real version transactions are exercised by release_tests.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
TMP=$(mktemp -d "${TMPDIR:-/tmp}/vellum-source-delegation.XXXXXX")
trap 'rm -rf -- "$TMP"' EXIT
SRC="$TMP/src"
mkdir -p "$SRC/tools" "$TMP/bin" "$TMP/home/.config/niri"
printf 'user bindings must remain\n' > "$TMP/home/.config/niri/config.kdl"
cp "$ROOT/install.sh" "$SRC/install.sh"
cp -R "$ROOT/contrib" "$SRC/contrib"
cp "$ROOT/tools/package_release.py" "$SRC/tools/package_release.py"
cp "$ROOT/LICENSE" "$SRC/LICENSE"
cat > "$TMP/bin/cargo" <<'CARGO'
#!/usr/bin/env bash
set -euo pipefail
mkdir -p "${VELLUM_TEST_SRC:?}/target/release"
for bin in vellum vellumctl vellum-ui vellum-tray; do
cat > "$VELLUM_TEST_SRC/target/release/$bin" <<'BINARY'
#!/usr/bin/env bash
# vellum-build-info-v1 marker: safe test binary, never a GUI.
if [[ ${1:-} == --build-info-json ]]; then
printf '%s\n' '{"format":"vellum-build-info-v1","version":"0.2.0","build_id":"source-wrapper-test","source_commit":"unknown","source_dirty":"unknown","source_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","target":"x86_64-unknown-linux-gnu","rustc":"rustc 1.90.0","profile":"release","config_schema":1,"ipc_schema":1}'
exit 0
fi
printf '%s\n' "$*" >> "${VELLUM_TEST_COMMANDS:?}"
[[ ${1:-} == release && ${2:-} == install ]] || exit 88
if [[ ${VELLUM_TEST_FAIL_ACTIVATION:-0} == 1 ]]; then
    echo '激活失败，已回退' >&2
    exit 7
fi
if ! systemctl --user daemon-reload; then
    echo '已安装，待激活：用户服务管理器不可达'
    exit 0
fi
echo 'ready'
BINARY
chmod 0755 "$VELLUM_TEST_SRC/target/release/$bin"
done
CARGO
cat > "$TMP/bin/systemctl" <<'SYSTEMCTL'
#!/usr/bin/env bash
echo 'Failed to connect to bus: No such file or directory' >&2
exit 1
SYSTEMCTL
chmod 0755 "$TMP/bin/cargo" "$TMP/bin/systemctl"
run_install() {
    PATH="$TMP/bin:/usr/bin:/bin" HOME="$TMP/home" \
    XDG_CONFIG_HOME="$TMP/home/.config" XDG_DATA_HOME="$TMP/home/.local/share" \
    VELLUM_BIN_DIR="$TMP/home/.local/bin" VELLUM_SKIP_PACKAGES=1 \
    VELLUM_TEST_COMMANDS="$TMP/commands" VELLUM_TEST_SRC="$SRC" \
    VELLUM_TEST_FAIL_ACTIVATION="${1:-0}" \
        bash "$SRC/install.sh"
}
run_install > "$TMP/stdout" 2> "$TMP/stderr"
grep -q '已安装，待激活' "$TMP/stdout"
grep -q 'release install --bundle' "$TMP/commands"
! grep -q 'shortcuts install' "$TMP/commands"
! grep -q '图标已加入系统托盘' "$TMP/stdout"
[[ -x "$SRC/target/release/vellum" ]]
[[ $(< "$TMP/home/.config/niri/config.kdl") == 'user bindings must remain' ]]
set +e
run_install 1 > "$TMP/failed-stdout" 2> "$TMP/failed-stderr"
code=$?
set -e
((code != 0))
grep -q '已回退' "$TMP/failed-stderr"
! grep -q '安装处理完成' "$TMP/failed-stdout"
echo 'source installer preserves pending status, propagates rollback failure and keeps build cache'
