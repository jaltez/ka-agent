#!/usr/bin/env bash
# Boundary gate (SEED.md §11.3): ka-* crates must stay inside the port.
# Modeled on ka's `cargo xtask deps`. Fails if any workspace member other
# than ka-agent-port and the app (apps/ka-agent) depends on a
# ka-engine / ka-protocol / ka-dialect / ka-strand crate.
set -euo pipefail

# Workspace members allowed to depend on ka-* crates.
ALLOWED="ka-agent-port ka-agent"

fail=0
while read -r member; do
    allowed=0
    for a in $ALLOWED; do
        if [[ "$member" == "$a" ]]; then
            allowed=1
            break
        fi
    done
    [[ $allowed -eq 1 ]] && continue

    offenders=$(cargo tree -p "$member" --edges normal 2>/dev/null \
        | grep -oE '^ka-(engine|protocol|dialect|strand) v[0-9]' \
        | awk '{print $1}' | sort -u || true)
    if [[ -n "$offenders" ]]; then
        echo "FAIL: workspace member '$member' depends on ka crate(s):" >&2
        echo "$offenders" | sed 's/^/  /' >&2
        echo "Only ka-agent-port and apps/ka-agent may depend on ka-* crates" >&2
        echo "(SEED.md §11.3). Keep Ka types behind the KaPort trait." >&2
        fail=1
    fi
done < <(cargo metadata --no-deps --format-version 1 \
    | python3 -c 'import json,sys
for p in json.load(sys.stdin)["packages"]:
    print(p["name"])')

if [[ $fail -ne 0 ]]; then
    exit 1
fi
echo "ok: no ka-* dependencies outside ka-agent-port and ka-agent"

# The pin constant the binary reports must match the manifest's git rev.
manifest_rev="$(grep -o 'rev = "[0-9a-f]\{40\}"' Cargo.toml | head -1 | cut -d'"' -f2)"
const_rev="$(grep -o 'KA_PIN_REV: &str = "[0-9a-f]\{40\}"' crates/ka-agent-port/src/lib.rs | cut -d'"' -f2)"
if [ "$manifest_rev" != "$const_rev" ]; then
  echo "error: ka-agent-port KA_PIN_REV ($const_rev) != Cargo.toml rev ($manifest_rev)"
  exit 1
fi
echo "ok: pin constant matches the manifest ($const_rev)"
