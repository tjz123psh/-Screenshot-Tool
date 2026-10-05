#!/usr/bin/env bash
# Binary-bundle entry point. No network, build, implicit legacy adoption or cleanup.
set -euo pipefail
bundle="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
exec "$bundle/bin/vellum" release install --bundle "$bundle" "$@"
