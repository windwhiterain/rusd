#!/usr/bin/env bash
# Build a compose manifest for the spike: cook examples/city.rusd, write the
# flattened stage's root layer as base64 usdc, add a usda session layer that
# overrides one lot's xformOp:translate, and emit the JSON a client sends.
#
# Nothing lands on disk but the cooked usdc and the manifest itself; the client
# that consumes it serves every layer to the resolver from memory.
#
# bash + coreutils + perl only: this shell's `python` is broken by pdm's
# PYTHONPATH (see AGENTS.md), so no python here.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"

out=${1:-target/viewer-spike/manifest.json}
mkdir -p "$(dirname "$out")"

cargo run -q -p rusd-cli --bin rusd -- export examples/city.rusd \
    target/viewer-spike/cooked.usdc >/dev/null

# base64 -w0: one line, no wrapping (a JSON string must not hold newlines).
data=$(base64 -w0 < target/viewer-spike/cooked.usdc)

# The session layer: four nested `over` clauses down to the lot, re-authoring
# xformOpOrder as token[] so the override lands on the one op the cook wrote.
# A prim clause nests by name from the pseudo-root (an absolute path is not
# spellable in one), which is why this is a staircase rather than one path.
session=$(cat <<'USDA'
#usda 1.0

over "World"
{
    over "City"
    {
        over "b0_0"
        {
            over "lot0_0"
            {
                uniform token[] xformOpOrder = ["xformOp:translate"]
                uniform point3f xformOp:translate = (3, 0, 7)
            }
        }
    }
}
USDA
)

# JSON-escape the session text in one pass: `\` first, then `"`, then a newline
# to the two-character sequence `\n`. perl -0777 so the whole text is one record.
session=$(printf '%s' "$session" | perl -0777 -pe 's/\\/\\\\/g; s/"/\\"/g; s/\n/\\n/g')

printf '%s' "{\"root\":\"/cooked/city.usdc\",\"session\":\"/session/override.usda\",\"layers\":[{\"identifier\":\"/session/override.usda\",\"encoding\":\"usda\",\"data\":\"$session\"},{\"identifier\":\"/cooked/city.usdc\",\"encoding\":\"usdc\",\"data\":\"$data\"}]}" > "$out"
echo "$out"