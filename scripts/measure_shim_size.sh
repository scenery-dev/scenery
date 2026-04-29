#!/usr/bin/env bash
# measure_shim_size.sh — measure the stripped release binary size of
# vectorhawkd-shim and verify it meets the M0 budget of <=3 MB.
#
# Usage:
#   bash scripts/measure_shim_size.sh
#
# Prerequisites:
#   cargo build --workspace --release  (or --release -p vectorhawkd-shim)
#
# Exit codes:
#   0  — binary size is within budget (<=3 MB)
#   1  — size exceeds budget, or the binary is not built
#
# Output files:
#   target/m0-shim-size.txt   — one line: "<value> bytes (<value_mb> MB)"

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SHIM_BIN="${REPO_ROOT}/target/release/vectorhawkd-shim"
SIZE_FILE="${REPO_ROOT}/target/m0-shim-size.txt"
MAX_SIZE_BYTES=$(( 3 * 1024 * 1024 ))   # 3 MB

# ── Preflight: binary must be built ──────────────────────────────────────────

if [[ ! -f "${SHIM_BIN}" ]]; then
    echo "ERROR: shim binary not built yet — run cargo build --workspace --release first" >&2
    echo "  Expected: ${SHIM_BIN}" >&2
    exit 1
fi

# ── Measure file size ─────────────────────────────────────────────────────────

# stat(1) differs between macOS BSD and GNU coreutils.
if stat --version 2>/dev/null | grep -q GNU; then
    SIZE_BYTES=$(stat --format="%s" "${SHIM_BIN}")
else
    # macOS BSD stat
    SIZE_BYTES=$(stat -f "%z" "${SHIM_BIN}")
fi

SIZE_MB=$(awk "BEGIN { printf \"%.2f\", ${SIZE_BYTES} / (1024 * 1024) }")

echo "INFO: shim binary size: ${SIZE_BYTES} bytes (${SIZE_MB} MB)"
echo "INFO: path: ${SHIM_BIN}"

# ── Write result file ─────────────────────────────────────────────────────────

mkdir -p "${REPO_ROOT}/target"
echo "${SIZE_BYTES} bytes (${SIZE_MB} MB)" > "${SIZE_FILE}"
echo "INFO: result written to ${SIZE_FILE}"

# ── Pass/fail gate ────────────────────────────────────────────────────────────

if [[ "${SIZE_BYTES}" -le "${MAX_SIZE_BYTES}" ]]; then
    echo "PASS: shim binary ${SIZE_MB} MB <= 3.00 MB"
    exit 0
else
    echo "FAIL: shim binary ${SIZE_MB} MB > 3.00 MB" >&2
    exit 1
fi
