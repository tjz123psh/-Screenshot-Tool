#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
TMP=$(mktemp -d "${TMPDIR:-/tmp}/vellum-remote-test.XXXXXX")
trap 'rm -rf -- "$TMP"' EXIT
mkdir -p "$TMP/bin" "$TMP/home"
cat > "$TMP/bin/git" <<'GIT'
#!/usr/bin/env bash
set -euo pipefail
[[ ${GIT_CONFIG_NOSYSTEM:-} == 1 && ${GIT_CONFIG_GLOBAL:-} == /dev/null && ${GIT_TERMINAL_PROMPT:-} == 0 ]] || exit 89
printf '%s\n' "$*" >> "${VELLUM_TEST_GIT_LOG:?}"
command=''; root=''; previous=''
for arg in "$@"; do
    if [[ "$previous" == -C ]]; then root="$arg"; fi
    case "$arg" in init|fetch|checkout|rev-parse|remote) command="$arg" ;; esac
    previous="$arg"
done
case "$command" in
    init) mkdir -p "${@: -1}/.git" ;;
    rev-parse) printf '%s\n' "${VELLUM_TEST_ACTUAL_REF:?}" ;;
    checkout)
        mkdir -p "$root/tools" "$root/crates/vellum-cli/src"
        : > "$root/tools/package_release.py"
        : > "$root/crates/vellum-cli/src/release.rs"
        printf '#!/usr/bin/env bash\n[[ $VELLUM_REMOTE_INSTALL == 1 ]] || exit 90\nprintf installed > "$VELLUM_TEST_INSTALL_MARKER"\n' > "$root/install.sh"
        ;;
esac
GIT
chmod 0755 "$TMP/bin/git"
REF=0123456789012345678901234567890123456789
run_remote() {
    PATH="$TMP/bin:/usr/bin:/bin" HOME="$TMP/home" TMPDIR="$TMP" \
    VELLUM_SOURCE_REF="$1" VELLUM_TEST_ACTUAL_REF="$2" \
    VELLUM_TEST_GIT_LOG="$TMP/git-args" VELLUM_TEST_INSTALL_MARKER="$TMP/installed" \
        bash "$ROOT/install-remote.sh"
}
if run_remote main "$REF" > "$TMP/out" 2> "$TMP/err"; then exit 1; fi
[[ ! -e "$TMP/git-args" && ! -e "$TMP/installed" ]]
if run_remote "$REF" 1111111111111111111111111111111111111111 > "$TMP/out" 2> "$TMP/err"; then exit 1; fi
[[ ! -e "$TMP/installed" ]]
run_remote "$REF" "$REF" > "$TMP/out" 2> "$TMP/err"
[[ $(< "$TMP/installed") == installed ]]
grep -q "fetch --quiet --depth=1 --no-tags origin $REF" "$TMP/git-args"
! grep -q 'refs/heads/main' "$TMP/git-args"
echo 'fixed source commit validated; moving refs/mismatches rejected before install'
