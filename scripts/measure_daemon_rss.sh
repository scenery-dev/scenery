#!/usr/bin/env bash
# measure_daemon_rss.sh — measure vectorhawkd idle RSS and verify it meets the
# M0 budget of <=50 MB.
#
# Usage:
#   bash scripts/measure_daemon_rss.sh
#
# Prerequisites:
#   cargo build --workspace --release  (or --release -p vectorhawkd-daemon)
#
# Exit codes:
#   0  — RSS is within budget (<=50 MB)
#   1  — RSS exceeds budget, or the binary is not built
#
# Output files:
#   target/m0-daemon-rss.txt   — one line: "<value_mb> MB"

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DAEMON_BIN="${REPO_ROOT}/target/release/vectorhawk"
RSS_FILE="${REPO_ROOT}/target/m0-daemon-rss.txt"
MAX_RSS_MB=50

# ── Preflight: binary must be built ──────────────────────────────────────────

if [[ ! -x "${DAEMON_BIN}" ]]; then
    echo "ERROR: vectorhawk binary not built yet — run cargo build --workspace --release first" >&2
    echo "  Expected: ${DAEMON_BIN}" >&2
    exit 1
fi

# ── Kill any stale daemon from a previous run ─────────────────────────────────

if pgrep -x vectorhawk > /dev/null 2>&1; then
    echo "INFO: killing stale vectorhawk daemon process(es) before measurement"
    pkill -x vectorhawk || true
    sleep 1
fi

# ── Spawn daemon in background ─────────────────────────────────────────────────

echo "INFO: spawning ${DAEMON_BIN} daemon run"
"${DAEMON_BIN}" daemon run &
DAEMON_PID=$!

# Give the daemon 2 seconds to settle into idle state before sampling RSS.
sleep 2

# ── Verify the process is still alive ─────────────────────────────────────────

if ! kill -0 "${DAEMON_PID}" 2>/dev/null; then
    echo "ERROR: daemon exited before RSS could be measured (PID ${DAEMON_PID})" >&2
    exit 1
fi

# ── Read RSS (kB on both macOS and Linux) ─────────────────────────────────────

RSS_KB=$(ps -o rss= -p "${DAEMON_PID}" 2>/dev/null | tr -d ' ')

if [[ -z "${RSS_KB}" ]]; then
    echo "ERROR: could not read RSS for PID ${DAEMON_PID}" >&2
    kill "${DAEMON_PID}" 2>/dev/null || true
    wait "${DAEMON_PID}" 2>/dev/null || true
    exit 1
fi

# Convert kB -> MB (integer division is fine for a budget check).
RSS_MB=$(( RSS_KB / 1024 ))
RSS_MB_FRAC=$(awk "BEGIN { printf \"%.1f\", ${RSS_KB} / 1024 }")

echo "INFO: daemon idle RSS: ${RSS_MB_FRAC} MB (${RSS_KB} kB, PID ${DAEMON_PID})"

# ── Write result file ─────────────────────────────────────────────────────────

mkdir -p "${REPO_ROOT}/target"
echo "${RSS_MB_FRAC} MB" > "${RSS_FILE}"
echo "INFO: result written to ${RSS_FILE}"

# ── Graceful shutdown ─────────────────────────────────────────────────────────

echo "INFO: sending SIGTERM to daemon (PID ${DAEMON_PID})"
kill -TERM "${DAEMON_PID}" 2>/dev/null || true

# Wait up to 5 seconds for clean exit.
for i in $(seq 1 10); do
    if ! kill -0 "${DAEMON_PID}" 2>/dev/null; then
        break
    fi
    sleep 0.5
done

if kill -0 "${DAEMON_PID}" 2>/dev/null; then
    echo "WARN: daemon did not exit within 5 s after SIGTERM; sending SIGKILL"
    kill -KILL "${DAEMON_PID}" 2>/dev/null || true
fi

wait "${DAEMON_PID}" 2>/dev/null || true

# ── Pass/fail gate ────────────────────────────────────────────────────────────

if [[ "${RSS_MB}" -le "${MAX_RSS_MB}" ]]; then
    echo "PASS: daemon idle RSS ${RSS_MB_FRAC} MB <= ${MAX_RSS_MB} MB"
    exit 0
else
    echo "FAIL: daemon idle RSS ${RSS_MB_FRAC} MB > ${MAX_RSS_MB} MB" >&2
    exit 1
fi
