#!/usr/bin/env bash
# measure_shim_size.sh — measure the release binary size of vectorhawk
# (the unified binary, which now embeds daemon and mcp-serve/shim functionality).
#
# Previously measured a separate vectorhawkd-shim binary. That binary no longer
# exists; mcp serve is now a subcommand of vectorhawk. This script now measures
# the unified vectorhawk binary and reports its size informally — the <=6 MB
# shim-specific budget no longer applies to this unified binary.
#
# Usage:
#   bash scripts/measure_shim_size.sh
#
# Prerequisites:
#   cargo build --workspace --release
#
# Exit codes:
#   0  — binary exists and size was measured
#   1  — binary is not built
#
# Output files:
#   target/m0-shim-size.txt   — one line: "<value> bytes (<value_mb> MB)"

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VECTORHAWK_BIN="${REPO_ROOT}/target/release/vectorhawk"
SIZE_FILE="${REPO_ROOT}/target/m0-shim-size.txt"

# ── Preflight: binary must be built ──────────────────────────────────────────

if [[ ! -f "${VECTORHAWK_BIN}" ]]; then
    echo "ERROR: vectorhawk binary not built yet — run cargo build --workspace --release first" >&2
    echo "  Expected: ${VECTORHAWK_BIN}" >&2
    exit 1
fi

# ── Measure file size ─────────────────────────────────────────────────────────

# stat(1) differs between macOS BSD and GNU coreutils.
if stat --version 2>/dev/null | grep -q GNU; then
    SIZE_BYTES=$(stat --format="%s" "${VECTORHAWK_BIN}")
else
    # macOS BSD stat
    SIZE_BYTES=$(stat -f "%z" "${VECTORHAWK_BIN}")
fi

SIZE_MB=$(awk "BEGIN { printf \"%.2f\", ${SIZE_BYTES} / (1024 * 1024) }")

echo "INFO: vectorhawk unified binary size: ${SIZE_BYTES} bytes (${SIZE_MB} MB)"
echo "INFO: path: ${VECTORHAWK_BIN}"
echo "INFO: note: this is the unified vectorhawk binary (embeds daemon + mcp serve)."
echo "INFO: the separate vectorhawkd-shim binary no longer exists."

# ── Write result file ─────────────────────────────────────────────────────────

mkdir -p "${REPO_ROOT}/target"
echo "${SIZE_BYTES} bytes (${SIZE_MB} MB)" > "${SIZE_FILE}"
echo "INFO: result written to ${SIZE_FILE}"

# ── Always pass (informational only) ─────────────────────────────────────────

echo "PASS: vectorhawk binary measured at ${SIZE_MB} MB (informational)"
exit 0
