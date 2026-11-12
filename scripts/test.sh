#!/usr/bin/env bash
# Runs the contract unit tests and the sandbox end-to-end tests.
#
# near-workspaces downloads a near-sandbox binary on first use. On hosts with
# glibc < 2.39 (e.g. Ubuntu 22.04) that binary does not start; point
# NEAR_SANDBOX_BIN_PATH at a working one, or drop a wrapper at
# .sandbox-bin/near-sandbox and it is picked up automatically.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
if [[ -z "${NEAR_SANDBOX_BIN_PATH:-}" && -x .sandbox-bin/near-sandbox ]]; then
  export NEAR_SANDBOX_BIN_PATH="$PWD/.sandbox-bin/near-sandbox"
fi
cargo test -p unwrapped
# One sandbox node at a time keeps memory and disk use low.
cargo test -p integration-tests -- --test-threads=1 "$@"
