#!/usr/bin/env bash
# vellum remote installer.
#
# Downloads the latest vellum source from GitHub and runs the in-tree
# install.sh.  Everything is staged in a temporary directory that is removed
# on every exit path, so a successful install leaves nothing behind except the
# binaries, systemd units, desktop entry, icons and the tray.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/tjz123psh/-Screenshot-Tool/main/install-remote.sh | bash
#
# install.sh switches pass through as environment variables:
#   VELLUM_SKIP_PACKAGES=1 VELLUM_SKIP_SHORTCUTS=1 VELLUM_SKIP_CLEANUP=1
set -euo pipefail

REPO="tjz123psh/-Screenshot-Tool"
BRANCH="main"
TARBALL_URL="https://github.com/$REPO/archive/refs/heads/$BRANCH.tar.gz"

info() { printf '\033[1;34m::\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || die "需要 curl（Arch: sudo pacman -S curl）"
command -v tar  >/dev/null 2>&1 || die "需要 tar（Arch: sudo pacman -S tar）"

# Stage the whole install in a temp dir.  The trap fires on success and on
# failure, so neither the source tree nor the cargo build tree ever survives.
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/vellum-install.XXXXXX")"
trap 'rm -rf -- "$tmp_dir"' EXIT

info "下载 vellum（$BRANCH 分支）..."
curl -fsSL --retry 3 "$TARBALL_URL" -o "$tmp_dir/vellum.tar.gz" \
    || die "下载失败：$TARBALL_URL"

info "解压源码..."
mkdir -p "$tmp_dir/src"
tar -xzf "$tmp_dir/vellum.tar.gz" -C "$tmp_dir/src" --strip-components=1
[[ -f "$tmp_dir/src/install.sh" ]] || die "下载的源码缺少 install.sh，已中止"

cd "$tmp_dir/src"
bash install.sh
