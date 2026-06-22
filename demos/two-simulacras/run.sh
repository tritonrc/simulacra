#!/usr/bin/env bash
# demos/two-simulacras/run.sh
# Builds the simulacra binary and launches the Bun bridge.
#
# Usage:
#   export ANTHROPIC_API_KEY="sk-ant-..."
#   ./run.sh
#
# Optional: pass bun args after --
#   ./run.sh --help

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# --clean wipes memory dirs for a fresh conversation
if [[ "${1:-}" == "--clean" ]]; then
  echo "▶  Cleaning memory dirs…"
  rm -rf "${SCRIPT_DIR}/acme-mem" "${SCRIPT_DIR}/maya-mem"
  shift
fi

echo "▶  Building simulacra-cli…"
cargo build \
  --manifest-path "${WORKSPACE_ROOT}/Cargo.toml" \
  -p simulacra-cli \
  --quiet

SIMULACRA_BIN="${WORKSPACE_ROOT}/target/debug/simulacra"
export SIMULACRA_BIN

echo "▶  Starting bridge…"
cd "${SCRIPT_DIR}"
exec bun run bridge.ts "$@"
