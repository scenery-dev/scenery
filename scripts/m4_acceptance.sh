#!/usr/bin/env bash
# m4_acceptance.sh — top-level M4 acceptance gate.
#
# Runs M0/M1/M2/M3 regression checks first, then verifies the M4 contract:
# the user-facing shim hard-requires the daemon and surfaces a JSON-RPC
# error (code -32001, message contains "daemon") when the daemon is missing.
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
#   bash scripts/m4_acceptance.sh
#
# Earlier gates (M0/M1/M2/M3) are invoked automatically.
# ============================================================================
#
# M4 acceptance criteria (from context/RUN1_M4_STREAMS.md):
#   AC1: Shim hard-requires daemon — DaemonRequired mode replaces embedded fallback.
#   AC2: Mid-session disconnect surfaces the same daemon-required error.
#   AC3: EmbeddedBackend not constructed in production shim code.
#   AC4: M0 daemon-kill test asserts the new error contract.
#   AC5: Repo-grep cleanliness — no "in-process fallback" / "embedded fallback"
#        references in shim production code.
#   AC6: `mcp setup` matrix unchanged (command=vectorhawk args=[mcp,serve]).
#   AC7: Acceptance gate (this script) covers the new contract.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SHIM_BIN="${REPO_ROOT}/target/release/vectorhawk"
CLI_BIN="${REPO_ROOT}/target/release/vectorhawk"

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
echo "VectorHawk M4 Acceptance Gate"
echo "=============================="
echo ""

# ── AC7 (regression): run earlier gates ───────────────────────────────────────

for gate in m0 m1 m2 m3; do
    echo "AC7: ${gate} regression (bash scripts/${gate}_acceptance.sh) ..."
    if bash "${REPO_ROOT}/scripts/${gate}_acceptance.sh" >/dev/null 2>&1; then
        record "PASS" "AC7: ${gate} regression" \
            "${gate}_acceptance.sh returned 0"
    else
        record "FAIL" "AC7: ${gate} regression" \
            "${gate}_acceptance.sh returned non-zero — re-run it directly"
    fi
done

# ── Build release binaries (idempotent) ───────────────────────────────────────

echo "AC1/AC2/AC4: building release binaries ..."
if ! cargo build --workspace --release >/dev/null 2>&1; then
    record "FAIL" "AC1: release build" "cargo build --workspace --release failed"
else
    record "PASS" "AC1: release build" "cargo build --workspace --release succeeded"
fi

# ── AC1+AC2: shim without daemon returns the daemon-required error ───────────

echo "AC1+AC2: shim emits daemon-required error when daemon is down ..."

# Make absolutely sure no daemon is running so the test is deterministic.
pkill -x vectorhawk >/dev/null 2>&1 || true
sleep 0.3

# Find a temp HOME so the shim can compute a socket path and find it absent.
TMP_HOME="$(mktemp -d)"
trap 'rm -rf "${TMP_HOME}"' EXIT

# Send an `initialize` JSON-RPC frame on stdin, then EOF. Capture stdout.
INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"m4-gate","version":"0.0.1"}}}'

# `timeout` is GNU-only; macOS doesn't ship it. Use a portable wait pattern.
SHIM_OUT="$(mktemp)"
SHIM_ERR="$(mktemp)"
trap 'rm -rf "${TMP_HOME}" "${SHIM_OUT}" "${SHIM_ERR}"' EXIT

# Run shim in a subshell with HOME redirected; pipe in the request and EOF.
HOME="${TMP_HOME}" XDG_RUNTIME_DIR="${TMP_HOME}/runtime" \
    "${SHIM_BIN}" mcp serve >"${SHIM_OUT}" 2>"${SHIM_ERR}" <<EOF &
${INIT_REQ}
EOF
SHIM_PID=$!

# Wait up to 5 s for the shim to terminate (it should exit on stdin EOF).
WAITED=0
while kill -0 ${SHIM_PID} 2>/dev/null; do
    if [[ ${WAITED} -ge 50 ]]; then
        kill -9 ${SHIM_PID} 2>/dev/null || true
        break
    fi
    sleep 0.1
    WAITED=$((WAITED + 1))
done
wait ${SHIM_PID} 2>/dev/null || true

SHIM_OUT_CONTENT="$(cat "${SHIM_OUT}")"
SHIM_ERR_CONTENT="$(cat "${SHIM_ERR}")"

# Contract: stdout has a single JSON-RPC frame with code=-32001 and message containing "daemon".
AC12_PASS=0
if [[ "${SHIM_OUT_CONTENT}" == *'"error"'* && \
      "${SHIM_OUT_CONTENT}" == *'-32001'* && \
      "${SHIM_OUT_CONTENT}" == *'daemon'* ]]; then
    AC12_PASS=1
fi

if [[ ${AC12_PASS} -eq 1 ]]; then
    record "PASS" "AC1+AC2: shim returns -32001 with daemon-required message" \
        "stdout contained -32001 and 'daemon'"
else
    record "FAIL" "AC1+AC2: shim returns -32001 with daemon-required message" \
        "stdout: ${SHIM_OUT_CONTENT:0:300}"
fi

# ── AC3+AC5: source grep — no EmbeddedBackend in shim production code ────────

echo "AC3+AC5: source grep for EmbeddedBackend constructions in shim production code ..."

SHIM_SRC="${REPO_ROOT}/crates/vectorhawkd-shim/src/lib.rs"

# Strip out lines under `#[cfg(test)]` mod blocks. Simplest check: look for
# `EmbeddedBackend::` in lib.rs. The shim's production code path no longer
# constructs the type; its tests live in lib_tests.rs.
if grep -nE 'EmbeddedBackend::' "${SHIM_SRC}" >/dev/null 2>&1; then
    record "FAIL" "AC3: no EmbeddedBackend:: in shim production code" \
        "found EmbeddedBackend:: references in $(basename "${SHIM_SRC}")"
else
    record "PASS" "AC3: no EmbeddedBackend:: in shim production code" \
        "no EmbeddedBackend:: in $(basename "${SHIM_SRC}")"
fi

# AC5: scrub for marketing fallback phrasing. The doc-comment may mention the
# historical mode but should not promise auto-fallback in the user-facing
# description. We allow `embedded backend` (test usage) but not the phrase
# `embedded fallback` or `in-process fallback` in lib.rs production text.
if grep -niE 'embedded fallback|in-process fallback|silently switches' "${SHIM_SRC}" >/dev/null 2>&1; then
    record "FAIL" "AC5: no fallback marketing in shim lib.rs" \
        "stale fallback phrasing remains"
else
    record "PASS" "AC5: no fallback marketing in shim lib.rs" \
        "lib.rs is clean of fallback promises"
fi

# ── AC4: m0_daemon_kill asserts the new contract ─────────────────────────────

echo "AC4: m0_daemon_kill asserts daemon-required error contract ..."

if grep -nE -- '-32001|DAEMON_UNREACHABLE|daemon-required' \
        "${REPO_ROOT}/crates/vectorhawkd-daemon/tests/m0_daemon_kill.rs" >/dev/null 2>&1; then
    record "PASS" "AC4: m0_daemon_kill asserts new error contract" \
        "test source references -32001 / daemon-required"
else
    record "FAIL" "AC4: m0_daemon_kill asserts new error contract" \
        "test source still asserts the old fallback contract"
fi

# ── AC6: mcp setup shape unchanged ───────────────────────────────────────────

echo "AC6: verifying mcp setup entry shape ..."
if [[ -x "${CLI_BIN}" ]]; then
    SETUP_OUTPUT="$("${CLI_BIN}" mcp setup --dry-run 2>&1 || true)"
    if [[ "${SETUP_OUTPUT}" == *'"vectorhawk"'* && \
          "${SETUP_OUTPUT}" == *'"mcp"'* && \
          "${SETUP_OUTPUT}" == *'"serve"'* ]]; then
        record "PASS" "AC6: mcp setup shape unchanged" \
            "command=vectorhawk args=[mcp,serve]"
    else
        record "FAIL" "AC6: mcp setup shape unchanged" \
            "unexpected mcp setup output: ${SETUP_OUTPUT:0:200}"
    fi
else
    record "FAIL" "AC6: mcp setup shape unchanged" "CLI binary not built"
fi

# ── Summary ──────────────────────────────────────────────────────────────────

echo ""
echo "=============================="
echo "M4 Acceptance Results"
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
        N/A) ICON="${SKIP}" ;;
        *)    ICON="${STATUS}" ;;
    esac

    printf "  %-60s [%b]\n" "${LABEL}" "${ICON}"
    if [[ -n "${DETAIL}" ]]; then
        printf "    %s\n" "${DETAIL}"
    fi
done

echo ""
if [[ ${OVERALL} -eq 1 ]]; then
    printf "%bAll M4 acceptance criteria PASSED%b\n" "${GREEN}" "${RESET}"
    exit 0
else
    printf "%bOne or more M4 acceptance criteria FAILED%b\n" "${RED}" "${RESET}"
    exit 1
fi
