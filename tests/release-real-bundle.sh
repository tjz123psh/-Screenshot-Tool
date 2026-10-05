#!/usr/bin/env bash
# Explicit native acceptance: real binaries and filesystem, no host service/UI.
# Requires unprivileged user/PID namespaces. Does not acquire host root privileges.
set -euo pipefail
if [[ ${1:-} != --inside ]]; then
    [[ $# == 1 ]] || { echo 'usage: release-real-bundle.sh /absolute/verified/bundle' >&2; exit 2; }
    BUNDLE=$(realpath -- "$1")
    ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
    unshare --user --map-root-user --pid --fork --mount-proc true || { echo 'native isolation unavailable; test not run' >&2; exit 77; }
    umask 077
    TMP=$(mktemp -d "${TMPDIR:-/tmp}/vellum-real-bundle.XXXXXX")
    trap 'rm -rf -- "$TMP"' EXIT
    printf 'owned native release fixture\n' > "$TMP/.fixture"
    mkdir -p "$TMP/home" "$TMP/runtime"
    unshare --user --map-root-user --pid --fork --mount-proc \
        env -i PATH=/usr/bin:/bin LANG=C.UTF-8 HOME="$TMP/home" \
        XDG_RUNTIME_DIR="$TMP/runtime" DBUS_SESSION_BUS_ADDRESS="unix:path=$TMP/no-bus" \
        bash "$ROOT/tests/release-real-bundle.sh" --inside "$BUNDLE" "$TMP"
    exit
fi
BUNDLE=$2
TMP=$3
[[ -f "$TMP/.fixture" && $(< "$TMP/.fixture") == 'owned native release fixture' ]]
INSTALL="$TMP/install root"
BIN="$TMP/public bin"
CONFIG="$TMP/config space"
DATA="$TMP/data space"
mkdir -p "$CONFIG/vellum" "$CONFIG/niri" "$DATA/user-images" "$TMP/home/.local/state/vellum/recovery"
printf 'user config sentinel\n' > "$CONFIG/vellum/config.toml"
printf 'user bindings sentinel\n' > "$CONFIG/niri/config.kdl"
printf 'user image sentinel\n' > "$DATA/user-images/keep.png"
printf 'recovery sentinel\n' > "$TMP/home/.local/state/vellum/recovery/keep.png"
sha256sum "$CONFIG/vellum/config.toml" "$CONFIG/niri/config.kdl" "$DATA/user-images/keep.png" "$TMP/home/.local/state/vellum/recovery/keep.png" > "$TMP/before"
MANAGER="$BUNDLE/bin/vellum"
ARGS=(--root "$INSTALL" --bin-dir "$BIN" --config-dir "$CONFIG" --data-dir "$DATA")
"$MANAGER" release install --bundle "$BUNDLE" --no-activate --json "${ARGS[@]}" > "$TMP/install.json"
python3 - "$TMP/install.json" "$BUNDLE/manifest.json" <<'PY'
import json,sys
r=json.load(open(sys.argv[1]));m=json.load(open(sys.argv[2]))
assert r['success'] is True and r['state']=='installed-pending-activation',r
assert r['current']==m['release_id'],r
PY
for name in vellum vellumctl vellum-ui vellum-tray; do
    [[ -L "$BIN/$name" ]]
    "$BIN/$name" --build-info-json > "$TMP/$name.json"
done
python3 - "$BUNDLE/manifest.json" "$TMP" <<'PY'
import json,sys,pathlib
m=json.load(open(sys.argv[1]));r=pathlib.Path(sys.argv[2])
for n in ['vellum','vellumctl','vellum-ui','vellum-tray']:
    assert json.load(open(r/(n+'.json')))==m['build'],n
PY
# Defaults must come from the actual managed instance, not the unrelated HOME.
"$BIN/vellum" release status --json > "$TMP/status.json"
python3 - "$TMP/status.json" <<'PY'
import json,sys
r=json.load(open(sys.argv[1]));assert r['state']=='installed-pending-activation',r
PY
"$MANAGER" release uninstall --yes --json "${ARGS[@]}" > "$TMP/uninstall.json"
python3 - "$TMP/uninstall.json" "$BUNDLE/manifest.json" "$INSTALL" "$BIN" "$CONFIG" "$DATA" <<'PY'
import json,os,pathlib,sys
r=json.load(open(sys.argv[1]));m=json.load(open(sys.argv[2]));root=pathlib.Path(sys.argv[3])
assert r['success'] is True and r['current'] is None,r
assert not (root/'releases'/m['release_id']).exists(),'program version retained unexpectedly'
for n in ['vellum','vellumctl','vellum-ui','vellum-tray']:
    assert not os.path.lexists(pathlib.Path(sys.argv[4])/n),n
for rel in ['systemd/user/vellum.service','systemd/user/vellum-tray.service','systemd/user/vellum-shortcuts.service','autostart/ai.vellum-shortcuts.desktop']:
    assert not os.path.lexists(pathlib.Path(sys.argv[5])/rel),rel
for rel in ['applications/ai.vellum.desktop','applications/ai.vellum-panel.desktop','dbus-1/services/ai.vellum.Shortcuts.service']:
    assert not os.path.lexists(pathlib.Path(sys.argv[6])/rel),rel
PY
sha256sum -c "$TMP/before"
echo 'real bundle: pending install, four binary identities, instance defaults and uninstall all verified; user data unchanged'
