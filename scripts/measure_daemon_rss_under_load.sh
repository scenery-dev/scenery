#!/usr/bin/env bash
# measure_daemon_rss_under_load.sh — measure peak vectorhawkd RSS under the
# M5 5-shim 1000-call stress load and verify it meets the <=100 MB budget.
#
# Usage:
#   bash scripts/measure_daemon_rss_under_load.sh
#
# Prerequisites:
#   cargo build --workspace --release  (or run with --build flag)
#
# Exit codes:
#   0  — peak RSS under load is within budget (<=100 MB)
#   1  — peak RSS exceeds budget, or build/test failed
#
# Output files:
#   target/m5-daemon-rss-under-load.txt  — key=value pairs written by the test

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DAEMON_BIN="${REPO_ROOT}/target/release/vectorhawk"
RSS_FILE="${REPO_ROOT}/target/m5-daemon-rss-under-load.txt"
MAX_UNDER_LOAD_MB=100

# ── Self-clean: kill stale daemon + remove socket (per 321ca06 pattern) ───────

if pgrep -x vectorhawk > /dev/null 2>&1; then
    echo "INFO: killing stale vectorhawk daemon process(es) before test"
    pkill -x vectorhawk || true
    sleep 1
fi

# Attempt to remove any stale socket.
if [[ "$(uname -s)" == "Darwin" ]]; then
    SOCK_PATH="${HOME}/Library/Application Support/VectorHawk/agent.sock"
else
    XDG_RT="${XDG_RUNTIME_DIR:-}"
    if [[ -n "${XDG_RT}" ]]; then
        SOCK_PATH="${XDG_RT}/vectorhawk/agent.sock"
    else
        SOCK_PATH="${HOME}/.local/share/VectorHawk/agent.sock"
    fi
fi

if [[ -e "${SOCK_PATH}" ]]; then
    echo "INFO: removing stale socket at ${SOCK_PATH}"
    rm -f "${SOCK_PATH}"
fi

# ── Build if needed ───────────────────────────────────────────────────────────

if [[ ! -x "${DAEMON_BIN}" ]]; then
    echo "INFO: daemon binary not found — building release workspace"
    cargo build --workspace --release
fi

# ── Run the M5 stress test (captures peak RSS to target/m5-daemon-rss-under-load.txt) ─

echo "INFO: running m5_stress_multi_shim integration test (this takes ~10-15 s) ..."
if ! cargo test --release -p vectorhawkd-daemon --test m5_stress_multi_shim \
        -- --include-ignored --nocapture 2>&1; then
    echo "FAIL: m5_stress_multi_shim test failed — cannot measure under-load RSS" >&2
    exit 1
fi

# ── Parse the RSS result file written by the test ─────────────────────────────

if [[ ! -f "${RSS_FILE}" ]]; then
    echo "FAIL: RSS result file not found at ${RSS_FILE}" >&2
    echo "      The test may not have written it — check test output above." >&2
    exit 1
fi

echo "INFO: parsing ${RSS_FILE}"
cat "${RSS_FILE}"

# Extract peak_mb value.
PEAK_MB=$(grep '^peak_mb=' "${RSS_FILE}" | cut -d= -f2 | tr -d '[:space:]')

if [[ -z "${PEAK_MB}" ]]; then
    echo "FAIL: could not parse peak_mb from ${RSS_FILE}" >&2
    exit 1
fi

echo "INFO: daemon under-load peak RSS: ${PEAK_MB} MB"

# ── Pass/fail gate ────────────────────────────────────────────────────────────

if [[ "${PEAK_MB}" -le "${MAX_UNDER_LOAD_MB}" ]]; then
    echo "PASS: daemon under-load peak RSS ${PEAK_MB} MB <= ${MAX_UNDER_LOAD_MB} MB"
    exit 0
else
    echo "FAIL: daemon under-load peak RSS ${PEAK_MB} MB > ${MAX_UNDER_LOAD_MB} MB" >&2
    exit 1
fi
