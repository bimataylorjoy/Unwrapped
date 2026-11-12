#!/usr/bin/env bash
# Builds every contract to target/near/<name>/<name>.wasm with cargo-near.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
for c in unwrapped mock-ft mock-signer mock-btc-client; do
  (cd "contracts/$c" && cargo near build non-reproducible-wasm --no-abi)
done
ls -la target/near/*/*.wasm
