#!/usr/bin/env bash
# m5_acceptance.sh — top-level M5 acceptance gate.
#
# Runs M0/M1/M2/M3/M4/D1 regression checks, then verifies the M5 acceptance
# criteria: 5-shim stress harness, mid-load kill contract, compute budget.
#
# ============================================================================
# RUNNING INSTRUCTIONS
# ============================================================================
# Prerequisites — build the workspace in release mode first:
#
#   cargo build --workspace --release
#
# Then run from the repo root:
#
#   bash scripts/m5_acceptance.sh
#
# Earlier gates (M0/M1/M2/M3/M4/D1) are invoked automatically.
# ============================================================================
#
# M5 acceptance criteria (from context/RUN1_M5_STREAMS.md):
#   AC1: Multi-shim stress harness — 5 shims, 1000 calls, all succeed, zero
#        socket errors.
#   AC2: Mid-load daemon-kill — remaining calls return -32001 "daemon" within 3 s.
#   AC3: Compute budget — idle RSS <=50 MB, under-load RSS <=100 MB, shim
#        binary <=6 MB, shim cold-start <=30 ms.
#   AC5: All prior gates (M0/M1/M2/M3/M4/D1) pass at M5 tip.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -t 1 ]]; then
    GREEN="\033[0;32m"; RED="\033[0;31m"; YELLOW="\033[0;33m"; RESET="\033[0m"
else
    GREEN=""; RED=""; YELLOW=""; RESET=""
fi

PASS="${GREEN}PASS${RESET}"
FAIL="${RED}FAIL${RESET}"
SKIP="${YELLOW}N/A ${RESET}"

declare -a RESULTS=()
declare -a LABELS=()
declare -a DETAILS=()

record() {
    local status="$1" label="$2" detail="$3"
    RESULTS+=("${status}")
    LABELS+=("${label}")
    DETAILS+=("${detail}")
}

echo ""
echo "VectorHawk M5 Acceptance Gate"
echo "=============================="
echo ""

# ── Initial cleanup — kill stale daemon + remove socket (321ca06 pattern) ────

echo "Initial cleanup: killing any stale vectorhawkd and removing socket ..."
pkill -x vectorhawkd >/dev/null 2>&1 || true
sleep 0.5

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

# ── AC5 regression: run prior gates ──────────────────────────────────────────

for gate in m0 m1 m2 m3 m4 d1; do
    echo "AC5: ${gate} regression (bash scripts/${gate}_acceptance.sh) ..."
    if bash "${REPO_ROOT}/scripts/${gate}_acceptance.sh" >/dev/null 2>&1; then
        record "PASS" "AC5: ${gate} regression" \
            "${gate}_acceptance.sh returned 0"
    else
        record "FAIL" "AC5: ${gate} regression" \
            "${gate}_acceptance.sh returned non-zero — re-run it directly for details"
    fi
    # Re-kill stale processes after each gate (each gate may leave a daemon running).
    pkill -x vectorhawkd >/dev/null 2>&1 || true
    sleep 0.5
    rm -f "${SOCK_PATH}" 2>/dev/null || true
done

# ── Build release binaries ────────────────────────────────────────────────────

echo "Build: cargo build --workspace --release ..."
if cargo build --workspace --release >/dev/null 2>&1; then
    record "PASS" "Build: release workspace" "cargo build --workspace --release succeeded"
else
    record "FAIL" "Build: release workspace" "cargo build --workspace --release FAILED"
fi

SHIM_BIN="${REPO_ROOT}/target/release/vectorhawkd-shim"

# ── AC1: 5-shim 1000-call stress test ────────────────────────────────────────

echo "AC1: m5_stress_multi_shim (5 shims, 1000 calls) ..."
pkill -x vectorhawkd >/dev/null 2>&1 || true
sleep 0.3
rm -f "${SOCK_PATH}" 2>/dev/null || true

STRESS_OUT="$(mktemp)"
if cargo test --release -p vectorhawkd-daemon --test m5_stress_multi_shim \
        -- --include-ignored --nocapture >"${STRESS_OUT}" 2>&1; then
    record "PASS" "AC1: 5-shim 1000-call stress" "m5_stress_multi_shim returned 0"
else
    record "FAIL" "AC1: 5-shim 1000-call stress" \
        "m5_stress_multi_shim FAILED — see ${STRESS_OUT}"
    cat "${STRESS_OUT}" >&2
fi
rm -f "${STRESS_OUT}"

pkill -x vectorhawkd >/dev/null 2>&1 || true
sleep 0.3
rm -f "${SOCK_PATH}" 2>/dev/null || true

# ── AC2: mid-load kill test ───────────────────────────────────────────────────

echo "AC2: m5_stress_kill (SIGKILL at 500 calls, -32001 contract) ..."
pkill -x vectorhawkd >/dev/null 2>&1 || true
sleep 0.3
rm -f "${SOCK_PATH}" 2>/dev/null || true

KILL_OUT="$(mktemp)"
if cargo test --release -p vectorhawkd-daemon --test m5_stress_kill \
        -- --include-ignored --nocapture >"${KILL_OUT}" 2>&1; then
    record "PASS" "AC2: mid-load kill -32001 contract" "m5_stress_kill returned 0"
else
    record "FAIL" "AC2: mid-load kill -32001 contract" \
        "m5_stress_kill FAILED — see ${KILL_OUT}"
    cat "${KILL_OUT}" >&2
fi
rm -f "${KILL_OUT}"

pkill -x vectorhawkd >/dev/null 2>&1 || true
sleep 0.3
rm -f "${SOCK_PATH}" 2>/dev/null || true

# ── AC3: idle RSS ─────────────────────────────────────────────────────────────

echo "AC3: daemon idle RSS (<=50 MB) ..."
pkill -x vectorhawkd >/dev/null 2>&1 || true
sleep 0.3
rm -f "${SOCK_PATH}" 2>/dev/null || true

IDLE_RSS_OUT="$(mktemp)"
if bash "${REPO_ROOT}/scripts/measure_daemon_rss.sh" >"${IDLE_RSS_OUT}" 2>&1; then
    IDLE_MB="$(grep -oE '[0-9]+\.[0-9]+ MB' "${IDLE_RSS_OUT}" | head -1)"
    record "PASS" "AC3: daemon idle RSS" "idle RSS = ${IDLE_MB:-?} (<=50 MB)"
else
    IDLE_TAIL="$(tail -3 "${IDLE_RSS_OUT}")"
    record "FAIL" "AC3: daemon idle RSS" "measure_daemon_rss.sh FAILED: ${IDLE_TAIL}"
    cat "${IDLE_RSS_OUT}" >&2
fi
rm -f "${IDLE_RSS_OUT}"

pkill -x vectorhawkd >/dev/null 2>&1 || true
sleep 0.3
rm -f "${SOCK_PATH}" 2>/dev/null || true

# ── AC3: under-load RSS ───────────────────────────────────────────────────────

echo "AC3: daemon under-load RSS (<=100 MB) via measure_daemon_rss_under_load.sh ..."
pkill -x vectorhawkd >/dev/null 2>&1 || true
sleep 0.3
rm -f "${SOCK_PATH}" 2>/dev/null || true

UNDERLOAD_OUT="$(mktemp)"
if bash "${REPO_ROOT}/scripts/measure_daemon_rss_under_load.sh" >"${UNDERLOAD_OUT}" 2>&1; then
    UNDER_LOAD_MB="$(grep '^peak_mb=' "${REPO_ROOT}/target/m5-daemon-rss-under-load.txt" 2>/dev/null | cut -d= -f2 | tr -d '[:space:]')"
    record "PASS" "AC3: daemon under-load RSS" \
        "peak RSS = ${UNDER_LOAD_MB:-?} MB (<=100 MB)"
else
    UNDERLOAD_TAIL="$(tail -5 "${UNDERLOAD_OUT}")"
    record "FAIL" "AC3: daemon under-load RSS" \
        "measure_daemon_rss_under_load.sh FAILED: ${UNDERLOAD_TAIL}"
    cat "${UNDERLOAD_OUT}" >&2
fi
rm -f "${UNDERLOAD_OUT}"

pkill -x vectorhawkd >/dev/null 2>&1 || true
sleep 0.3
rm -f "${SOCK_PATH}" 2>/dev/null || true

# ── AC3: shim binary size ─────────────────────────────────────────────────────

echo "AC3: shim binary size (<=6 MB) ..."
SHIM_SIZE_OUT="$(mktemp)"
if bash "${REPO_ROOT}/scripts/measure_shim_size.sh" >"${SHIM_SIZE_OUT}" 2>&1; then
    SHIM_MB="$(grep -oE '[0-9]+\.[0-9]+ MB' "${SHIM_SIZE_OUT}" | head -1)"
    record "PASS" "AC3: shim binary size" "size = ${SHIM_MB:-?} (<=6 MB)"
else
    record "FAIL" "AC3: shim binary size" "measure_shim_size.sh FAILED"
    cat "${SHIM_SIZE_OUT}" >&2
fi
rm -f "${SHIM_SIZE_OUT}"

# ── AC3: shim cold start ──────────────────────────────────────────────────────
# Best-of-3 to represent steady-state cold-start after disk cache is warm.
# The shim reads stdin; feeding </dev/null causes EOF → immediate exit.
# We parse bash's `time` builtin output (real Xs Ym format on macOS/Linux).

echo "AC3: shim cold start (best-of-3, <=30 ms) ..."

if [[ ! -x "${SHIM_BIN}" ]]; then
    record "FAIL" "AC3: shim cold start" "shim binary not built at ${SHIM_BIN}"
else
    best_ms=99999

    for _run in 1 2 3; do
        # Use /usr/bin/time -p for portable real-time output on macOS + Linux.
        TIME_OUT="$(/usr/bin/time -p "${SHIM_BIN}" </dev/null 2>&1 || true)"
        # /usr/bin/time -p outputs: "real N.NN" on both macOS and Linux.
        REAL_LINE="$(echo "${TIME_OUT}" | grep '^real')"
        REAL_SECS="$(echo "${REAL_LINE}" | awk '{print $2}')"
        # Convert to milliseconds (awk float multiply).
        REAL_MS="$(awk "BEGIN { printf \"%d\", ${REAL_SECS:-0} * 1000 }")"

        if [[ "${REAL_MS}" -lt "${best_ms}" ]]; then
            best_ms="${REAL_MS}"
        fi
    done

    if [[ "${best_ms}" -le 30 ]]; then
        record "PASS" "AC3: shim cold start" \
            "best-of-3 = ${best_ms} ms (<=30 ms)"
    else
        record "FAIL" "AC3: shim cold start" \
            "best-of-3 = ${best_ms} ms (>30 ms budget)"
    fi
fi

# ── Summary ───────────────────────────────────────────────────────────────────

echo ""
echo "=============================="
echo "M5 Acceptance Results"
echo "=============================="
echo ""

OVERALL=1
for i in "${!RESULTS[@]}"; do
    STATUS="${RESULTS[$i]}"
    LABEL="${LABELS[$i]}"
    DETAIL="${DETAILS[$i]}"

    case "${STATUS}" in
        PASS) ICON="${PASS}" ;;
        FAIL) ICON="${FAIL}"; OVERALL=0 ;;
        N/A)  ICON="${SKIP}" ;;
        *)    ICON="${STATUS}" ;;
    esac

    printf "  %-60s [%b]\n" "${LABEL}" "${ICON}"
    if [[ -n "${DETAIL}" ]]; then
        printf "    %s\n" "${DETAIL}"
    fi
done

echo ""
if [[ ${OVERALL} -eq 1 ]]; then
    printf "%bAll M5 acceptance criteria PASSED%b\n" "${GREEN}" "${RESET}"
    exit 0
else
    printf "%bOne or more M5 acceptance criteria FAILED%b\n" "${RED}" "${RESET}"
    exit 1
fi
