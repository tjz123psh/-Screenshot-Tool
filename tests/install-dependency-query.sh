#!/usr/bin/env bash
# A failed pacman query must stop before cargo or any install mutation runs.
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
TMP=$(mktemp -d "${TMPDIR:-/tmp}/vellum-install-query.XXXXXX")
trap 'rm -rf -- "$TMP"' EXIT
mkdir -p "$TMP/bin" "$TMP/home"

cat > "$TMP/bin/pacman" <<'PACMAN'
#!/usr/bin/env bash
echo 'package database unavailable' >&2
exit 42
PACMAN
cat > "$TMP/bin/cargo" <<'CARGO'
#!/usr/bin/env bash
: > "${VELLUM_TEST_CARGO_MARKER:?}"
exit 99
CARGO
chmod 0755 "$TMP/bin/pacman" "$TMP/bin/cargo"

set +e
PATH="$TMP/bin:/usr/bin:/bin" \
HOME="$TMP/home" \
VELLUM_BIN_DIR="$TMP/home/.local/bin" \
VELLUM_SKIP_SHORTCUTS=1 \
VELLUM_TEST_CARGO_MARKER="$TMP/cargo-ran" \
    bash "$ROOT/install.sh" > "$TMP/stdout" 2> "$TMP/stderr"
status=$?
set -e

if ((status == 0)); then
    echo 'installer unexpectedly accepted a failed pacman query' >&2
    exit 1
fi
if [[ -e "$TMP/cargo-ran" ]]; then
    echo 'installer continued to cargo after the pacman query failed' >&2
    exit 1
fi
if ! grep -q '依赖查询失败' "$TMP/stderr"; then
    echo 'installer did not report the pacman query failure' >&2
    exit 1
fi

# A normal "missing dependency" result may also carry warnings on stderr.
# Only stdout contains package names; diagnostics must never become pacman -S
# arguments.
WARNING_CASE="$TMP/missing-with-warning"
mkdir -p "$WARNING_CASE/bin" "$WARNING_CASE/home"

cat > "$WARNING_CASE/bin/pacman" <<'PACMAN'
#!/usr/bin/env bash
if [[ ${1:-} == "-T" ]]; then
    echo 'grim'
    echo 'warning: ignored test warning' >&2
    exit 127
fi
printf '%s\n' "$@" > "${VELLUM_TEST_PACMAN_ARGS:?}"
exit 42
PACMAN
cat > "$WARNING_CASE/bin/sudo" <<'SUDO'
#!/usr/bin/env bash
exec "$@"
SUDO
cat > "$WARNING_CASE/bin/cargo" <<'CARGO'
#!/usr/bin/env bash
: > "${VELLUM_TEST_CARGO_MARKER:?}"
exit 99
CARGO
chmod 0755 "$WARNING_CASE/bin/pacman" "$WARNING_CASE/bin/sudo" "$WARNING_CASE/bin/cargo"

set +e
PATH="$WARNING_CASE/bin:/usr/bin:/bin" \
HOME="$WARNING_CASE/home" \
VELLUM_BIN_DIR="$WARNING_CASE/home/.local/bin" \
VELLUM_SKIP_SHORTCUTS=1 \
VELLUM_TEST_PACMAN_ARGS="$WARNING_CASE/pacman-args" \
VELLUM_TEST_CARGO_MARKER="$WARNING_CASE/cargo-ran" \
    bash "$ROOT/install.sh" > "$WARNING_CASE/stdout" 2> "$WARNING_CASE/stderr"
status=$?
set -e

if ((status == 0)); then
    echo 'installer unexpectedly completed after the fake package install failed' >&2
    exit 1
fi
if [[ -e "$WARNING_CASE/cargo-ran" ]]; then
    echo 'installer continued to cargo after the fake package install failed' >&2
    exit 1
fi
if [[ ! -s "$WARNING_CASE/pacman-args" ]]; then
    echo 'installer did not attempt to install the reported missing package' >&2
    exit 1
fi
if ! grep -Fxq 'grim' "$WARNING_CASE/pacman-args"; then
    echo 'installer lost the missing package reported on stdout' >&2
    exit 1
fi
if grep -Fxq 'warning: ignored test warning' "$WARNING_CASE/pacman-args"; then
    echo 'installer passed pacman stderr diagnostics back as a package name' >&2
    exit 1
fi
