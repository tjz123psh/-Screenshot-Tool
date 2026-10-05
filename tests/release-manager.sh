#!/usr/bin/env bash
# Fault tests use temporary roots and injected service hooks, never real installs.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$ROOT"

# Fail rather than reporting success if the manager test module was not wired in.
listing=$(cargo test -p vellum-cli --locked --offline release::tests:: -- --list)
if [[ "$listing" != *"release::tests::"* ]]; then
    printf '%s\n' 'release manager test module is missing; refusing a zero-test success' >&2
    exit 1
fi
cargo test -p vellum-cli --locked --offline release::tests:: -- --test-threads=1
