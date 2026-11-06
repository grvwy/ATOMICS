#!/usr/bin/env bash
# No Nix or root required: run the real boot script against sample cidata and command stand-ins.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
python3 "$root/nix/tests/mac-shares-test.py" "$root/nix/vm/mac-shares.sh"
