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
        | grep -oE '^ka-(engine|protocol|dialect|strand|sandbox) v[0-9]' \
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

# The pin constant the binary reports must match the manifest's pin:
# the `rev` of the git pin, or the `=version` of a registry pin.
if grep -q 'ka-protocol = { git =' Cargo.toml; then
  manifest_pin="$(grep -oE 'ka-protocol = \{ git = "[^"]+", rev = "[0-9a-f]+" \}' Cargo.toml \
    | grep -oE 'rev = "[0-9a-f]+"' | sed -E 's/^rev = "([0-9a-f]+)"$/\1/')"
else
  manifest_pin="$(grep -oE 'ka-protocol = \{ version = "=[^"]+"' Cargo.toml | grep -oE '"=[^"]+"' | tr -d '"=' | head -1)"
fi
const_pin="$(grep -o 'KA_PIN: &str = "[^"]*"' crates/ka-agent-port/src/lib.rs | cut -d'"' -f2)"
if [ -z "$manifest_pin" ]; then
  echo "error: no exact ka-protocol pin (git rev or =version) found in Cargo.toml"
  exit 1
fi
if [ "$manifest_pin" != "$const_pin" ]; then
  echo "error: ka-agent-port KA_PIN ($const_pin) != Cargo.toml ka-protocol pin ($manifest_pin)"
  exit 1
fi
echo "ok: pin constant matches the manifest ($const_pin)"

# No local file:// mirrors may survive in any manifest.
if grep -rn 'file:///' Cargo.toml crates/*/Cargo.toml apps/*/Cargo.toml examples/plugins/probe/Cargo.toml 2>/dev/null; then
  echo "error: file:// dependency mirror left in a manifest"
  exit 1
fi
echo "ok: no file:// mirrors in manifests"
