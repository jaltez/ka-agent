#!/usr/bin/env bash
# Regenerate docs/schema/ka-agent.schema.json from the config serde model
# (A16). Run after changing crates/ka-agent-core/src/config.rs; CI runs
# this and fails when the committed document differs.
set -euo pipefail
cd "$(dirname "$0")/.."

out=docs/schema/ka-agent.schema.json
mkdir -p "$(dirname "$out")"
cargo run --quiet -p ka-agent -- config schema --json-schema > "$out.tmp"
mv "$out.tmp" "$out"
echo "wrote $out"
