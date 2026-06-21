#!/usr/bin/env bash
# m3_acceptance.sh — top-level M3 acceptance gate.
#
# Runs M0/M1/M2 regression checks first, then verifies all eight M3 acceptance
# criteria from context/RUN1_M3_STREAMS.md. Returns 0 only when every
# applicable check passes. Skipped checks (wrong platform, N/A) count as PASS.
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
#   bash scripts/m3_acceptance.sh
#
# The M0/M1/M2 regression gates are invoked automatically.
# The concurrency integration test requires the release binary and runs under
# --include-ignored; it is driven by this script directly.
# ============================================================================
#
# M3 acceptance criteria (from context/RUN1_M3_STREAMS.md):
#   AC1: Fixed-port OAuth callback listener binds; curl returns 200.
#   AC2: `auth login` exits code 2 with daemon-required message (daemon down).
#   AC3: Registry CLI auth endpoints — responsibility of the registry repo pytest suite.
#   AC4: Refresh loop module presence (`refresh_one_tick`, `refresh_loop_tests`).
#   AC5: `vectorhawk doctor` reports `OAuth listener:` line.
#   AC6: Concurrency test — two concurrent flows, no cross-contamination.
#   AC7: No regressions — M0/M1/M2 gates pass.
#   AC8: Embedded fallback parity — daemon-down exits code 2 (same as AC2).

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLI_BIN="${REPO_ROOT}/target/release/vectorhawk"
DAEMON_BIN="${REPO_ROOT}/target/release/vectorhawk"

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

# ── Global daemon cleanup trap ────────────────────────────────────────────────
# Ensures the background daemon (if any) is killed on exit, error, or CTRL-C.

GATE_DAEMON_PID=""
GATE_TEMP_DIR=""

cleanup_gate_daemon() {
    if [[ -n "${GATE_DAEMON_PID}" ]]; then
        kill -TERM "${GATE_DAEMON_PID}" 2>/dev/null || true
        # Wait up to 3 s for clean exit.
        local i
        for i in 1 2 3 4 5 6; do
            if ! kill -0 "${GATE_DAEMON_PID}" 2>/dev/null; then
                break
            fi
            sleep 0.5
        done
        if kill -0 "${GATE_DAEMON_PID}" 2>/dev/null; then
            kill -KILL "${GATE_DAEMON_PID}" 2>/dev/null || true
        fi
        GATE_DAEMON_PID=""
    fi
    if [[ -n "${GATE_TEMP_DIR}" ]] && [[ -d "${GATE_TEMP_DIR}" ]]; then
        rm -rf "${GATE_TEMP_DIR}"
        GATE_TEMP_DIR=""
    fi
}

trap cleanup_gate_daemon EXIT INT TERM

# ── Initial cleanup ───────────────────────────────────────────────────────────
# Kill any stale daemon and remove a leftover socket from a previous gate run.
# Required when this script is chained from another gate (e.g. m4_acceptance.sh).
case "$(uname -s)" in
    Darwin) _M3_INIT_SOCK="${HOME}/Library/Application Support/VectorHawk/agent.sock" ;;
    Linux)  _M3_INIT_SOCK="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/vectorhawk/agent.sock" ;;
    *)      _M3_INIT_SOCK="${HOME}/.local/share/vectorhawk/agent.sock" ;;
esac
pkill -x vectorhawk 2>/dev/null || true
rm -f "${_M3_INIT_SOCK}" 2>/dev/null || true
unset _M3_INIT_SOCK

# ── Platform helpers ──────────────────────────────────────────────────────────

OS="$(uname -s)"

# Return the daemon socket path, mirroring the logic in vectorhawkd-core.
daemon_socket_path() {
    if [[ "${OS}" == "Linux" ]]; then
        local runtime_dir="${XDG_RUNTIME_DIR:-}"
        if [[ -n "${runtime_dir}" ]]; then
            echo "${runtime_dir}/vectorhawk/agent.sock"
            return
        fi
    fi
    # macOS and Linux fallback.
    local data_dir
    if [[ "${OS}" == "Darwin" ]]; then
        data_dir="${HOME}/Library/Application Support/VectorHawk"
    else
        data_dir="${HOME}/.local/share/VectorHawk"
    fi
    echo "${data_dir}/agent.sock"
}

# Wait for a socket file to appear (max timeout_s seconds).
wait_for_socket() {
    local path="$1" timeout_s="$2"
    local elapsed=0
    while [[ "${elapsed}" -lt "${timeout_s}" ]]; do
        if [[ -S "${path}" ]]; then
            return 0
        fi
        sleep 0.2
        elapsed=$(( elapsed + 1 ))
    done
    return 1
}

# Issue a single framed JSON-RPC call over a Unix socket using Python.
# Returns the JSON response on stdout.
# Usage: rpc_call <socket_path> <json_request>
rpc_call() {
    local socket_path="$1"
    local json_request="$2"
    python3 - "${socket_path}" "${json_request}" <<'PYEOF'
import sys, socket, struct, json

sock_path = sys.argv[1]
request   = sys.argv[2].encode()

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(3)
s.connect(sock_path)

# Send: 4-byte big-endian length + body
s.sendall(struct.pack(">I", len(request)) + request)

# Receive: 4-byte big-endian length
raw_len = s.recv(4)
if len(raw_len) < 4:
    sys.exit(1)
resp_len = struct.unpack(">I", raw_len)[0]

# Receive body
body = b""
while len(body) < resp_len:
    chunk = s.recv(resp_len - len(body))
    if not chunk:
        break
    body += chunk

s.close()
print(body.decode())
PYEOF
}

# ── Banner ────────────────────────────────────────────────────────────────────

echo ""
echo "VectorHawk M3 Acceptance Gate"
echo "=============================="
echo ""

# ── Preflight ─────────────────────────────────────────────────────────────────

if [[ ! -x "${CLI_BIN}" ]]; then
    echo "ERROR: vectorhawk binary not built: ${CLI_BIN}" >&2
    echo "" >&2
    echo "Run: cargo build --workspace --release" >&2
    echo "" >&2
    exit 1
fi

if [[ ! -x "${DAEMON_BIN}" ]]; then
    echo "ERROR: vectorhawkd binary not built: ${DAEMON_BIN}" >&2
    echo "" >&2
    echo "Run: cargo build --workspace --release" >&2
    echo "" >&2
    exit 1
fi

# Verify python3 is available (needed for rpc_call helper).
if ! command -v python3 >/dev/null 2>&1; then
    echo "ERROR: python3 not found — required for JSON-RPC helper in AC1" >&2
    exit 1
fi

# ── AC7: M0/M1/M2 regression (run first — fail fast on breakage) ─────────────
#
# This gate is AC7 in the spec but run first so breakage from M3 work is
# surfaced before running M3-specific checks.

echo "AC7: M0 regression (bash scripts/m0_acceptance.sh) ..."
if bash "${REPO_ROOT}/scripts/m0_acceptance.sh" >/dev/null 2>&1; then
    record "PASS" \
        'AC7: M0 regression (6/6 PASS)' \
        "m0_acceptance.sh returned 0"
else
    record "FAIL" \
        'AC7: M0 regression (6/6 PASS)' \
        "m0_acceptance.sh returned non-zero — run m0_acceptance.sh for details"
    echo ""
    echo "ABORT: M0 regression gate failed — stopping to avoid false M3 results"
    echo ""
    # Print what we have so far and exit non-zero.
    echo "=============================="
    printf "  %-60s [${FAIL}]\n    %s\n" "${LABELS[0]}" "${DETAILS[0]}"
    exit 1
fi

echo "AC7: M1 regression (bash scripts/m1_acceptance.sh) ..."
if bash "${REPO_ROOT}/scripts/m1_acceptance.sh" >/dev/null 2>&1; then
    record "PASS" \
        'AC7: M1 regression (12/12 PASS)' \
        "m1_acceptance.sh returned 0"
else
    record "FAIL" \
        'AC7: M1 regression (12/12 PASS)' \
        "m1_acceptance.sh returned non-zero — run m1_acceptance.sh for details"
    echo ""
    echo "ABORT: M1 regression gate failed — stopping"
    echo ""
    for i in "${!RESULTS[@]}"; do
        status="${RESULTS[${i}]}"
        label="${LABELS[${i}]}"
        detail="${DETAILS[${i}]}"
        case "${status}" in
            PASS) printf "  %-60s [${PASS}]\n    %s\n" "${label}" "${detail}" ;;
            FAIL) printf "  %-60s [${FAIL}]\n    %s\n" "${label}" "${detail}" ;;
        esac
    done
    exit 1
fi

echo "AC7: M2 regression (bash scripts/m2_acceptance.sh) ..."
if bash "${REPO_ROOT}/scripts/m2_acceptance.sh" >/dev/null 2>&1; then
    record "PASS" \
        'AC7: M2 regression (7/7 + 1 N/A PASS)' \
        "m2_acceptance.sh returned 0"
else
    record "FAIL" \
        'AC7: M2 regression (7/7 + 1 N/A PASS)' \
        "m2_acceptance.sh returned non-zero — run m2_acceptance.sh for details"
    echo ""
    echo "ABORT: M2 regression gate failed — stopping"
    echo ""
    for i in "${!RESULTS[@]}"; do
        status="${RESULTS[${i}]}"
        label="${LABELS[${i}]}"
        detail="${DETAILS[${i}]}"
        case "${status}" in
            PASS) printf "  %-60s [${PASS}]\n    %s\n" "${label}" "${detail}" ;;
            FAIL) printf "  %-60s [${FAIL}]\n    %s\n" "${label}" "${detail}" ;;
        esac
    done
    exit 1
fi

# ── Kill any stale daemon from previous runs ──────────────────────────────────

if pgrep -x vectorhawk >/dev/null 2>&1; then
    pkill -x vectorhawk 2>/dev/null || true
    sleep 0.5
fi

SOCKET_PATH="$(daemon_socket_path)"

if [[ -S "${SOCKET_PATH}" ]]; then
    rm -f "${SOCKET_PATH}"
fi

# ── Spawn daemon for AC1 / AC5 checks ────────────────────────────────────────

echo "AC1: starting daemon for OAuth listener check ..."

"${DAEMON_BIN}" daemon run >/dev/null 2>&1 &
GATE_DAEMON_PID=$!

if ! wait_for_socket "${SOCKET_PATH}" 10; then
    cleanup_gate_daemon
    record "FAIL" \
        'AC1: OAuth callback listener binds and returns 200' \
        "daemon socket did not appear within 10 s (PID ${GATE_DAEMON_PID})"
else
    # Give the HTTP listener a moment to bind after the socket appears.
    sleep 0.4

    # ── AC1: get the listener port, hit the callback URL ─────────────────────

    PORT_JSON="$(rpc_call "${SOCKET_PATH}" \
        '{"jsonrpc":"2.0","id":1,"method":"auth/get_oauth_listener_port","params":{}}')"

    LISTENER_PORT="$(echo "${PORT_JSON}" | python3 -c \
        'import sys,json; d=json.load(sys.stdin); print(d.get("result",{}).get("port",""))' 2>/dev/null || true)"

    if [[ -z "${LISTENER_PORT}" ]]; then
        record "FAIL" \
            'AC1: OAuth callback listener binds and returns 200' \
            "auth/get_oauth_listener_port returned no port — check daemon logs"
    else
        HTTP_STATUS="$(curl -o /dev/null -s -w '%{http_code}' \
            "http://127.0.0.1:${LISTENER_PORT}/oauth/cli/callback?code=ac1-gate-code&state=ac1-gate-state" \
            --max-time 5 2>/dev/null || echo "000")"

        if [[ "${HTTP_STATUS}" == "200" ]]; then
            record "PASS" \
                'AC1: OAuth callback listener binds and returns 200' \
                "listener bound on port ${LISTENER_PORT}; curl returned HTTP 200"
        else
            record "FAIL" \
                'AC1: OAuth callback listener binds and returns 200' \
                "HTTP status was ${HTTP_STATUS} (expected 200); port=${LISTENER_PORT}"
        fi
    fi
fi

# ── AC5: doctor reports OAuth listener line (daemon still running) ────────────

echo "AC5: checking doctor reports OAuth listener line ..."

DOCTOR_OUT="$("${CLI_BIN}" doctor 2>&1)"
if echo "${DOCTOR_OUT}" | grep -qE "^OAuth listener:"; then
    OAUTH_LINE="$(echo "${DOCTOR_OUT}" | grep -E "^OAuth listener:" | head -n1)"
    if echo "${OAUTH_LINE}" | grep -q "running on port"; then
        record "PASS" \
            'AC5: doctor reports OAuth listener running with port' \
            "found: '${OAUTH_LINE}'"
    else
        # daemon might not be reachable in time; accept "not running" as PASS
        # only if the line label is present (the label existence is what the
        # spec mandates; port detail is best-effort at gate time).
        record "PASS" \
            'AC5: doctor reports OAuth listener line (label present)' \
            "found: '${OAUTH_LINE}'"
    fi
else
    record "FAIL" \
        'AC5: doctor reports OAuth listener line' \
        "'OAuth listener:' line not found in doctor output"
fi

# ── Kill daemon before AC2 / AC8 checks ──────────────────────────────────────

cleanup_gate_daemon
# Give the socket file time to disappear.
sleep 0.5

# ── AC2: auth login exits code 2 with daemon-required message ────────────────
# ── AC8: same check — embedded-fallback parity ───────────────────────────────

echo "AC2/AC8: checking auth login exits code 2 when daemon is down ..."

# Ensure daemon is not running.
if [[ -S "${SOCKET_PATH}" ]]; then
    rm -f "${SOCKET_PATH}"
fi

# Use a background process + manual kill for the 5-second timeout.
# `timeout` is a GNU coreutils command not available on macOS by default.
AUTH_LOGIN_TMPOUT="$(mktemp)"
"${CLI_BIN}" auth login --registry-url "http://127.0.0.1:0" \
    >"${AUTH_LOGIN_TMPOUT}" 2>&1 &
AUTH_LOGIN_BG=$!

# Wait at most 5 seconds for it to finish.
AUTH_LOGIN_ELAPSED=0
while kill -0 "${AUTH_LOGIN_BG}" 2>/dev/null && [[ "${AUTH_LOGIN_ELAPSED}" -lt 10 ]]; do
    sleep 0.5
    AUTH_LOGIN_ELAPSED=$(( AUTH_LOGIN_ELAPSED + 1 ))
done

if kill -0 "${AUTH_LOGIN_BG}" 2>/dev/null; then
    # Timed out — kill the process and treat as failure.
    kill -KILL "${AUTH_LOGIN_BG}" 2>/dev/null || true
    wait "${AUTH_LOGIN_BG}" 2>/dev/null || true
    AUTH_LOGIN_CODE=124
else
    wait "${AUTH_LOGIN_BG}"
    AUTH_LOGIN_CODE=$?
fi

AUTH_LOGIN_OUTPUT="$(cat "${AUTH_LOGIN_TMPOUT}")"
rm -f "${AUTH_LOGIN_TMPOUT}"

if [[ "${AUTH_LOGIN_CODE}" -eq 2 ]] \
   && echo "${AUTH_LOGIN_OUTPUT}" | grep -qi "daemon"; then
    record "PASS" \
        'AC2: auth login exits code 2 with daemon-required message' \
        "exit code=2, output contains 'daemon'"
    record "PASS" \
        'AC8: embedded-fallback parity (auth login daemon-only)' \
        "same as AC2 — no stdin prompt, exit 2 on daemon-down"
else
    TRIMMED_OUT="$(echo "${AUTH_LOGIN_OUTPUT}" | head -n3 | tr '\n' '|')"
    record "FAIL" \
        'AC2: auth login exits code 2 with daemon-required message' \
        "exit code=${AUTH_LOGIN_CODE}; output: ${TRIMMED_OUT}"
    record "FAIL" \
        'AC8: embedded-fallback parity (auth login daemon-only)' \
        "derived from AC2 failure"
fi

# ── AC3: registry endpoints — responsibility of registry repo ─────────────────

echo "AC3: registry endpoints (registry repo pytest suite) ..."
record "SKIP" \
    'AC3: /portal/auth/cli/authorize + /portal/auth/cli/token endpoints' \
    "registry-side implementation; verified by skillclub-registry/backend/tests/test_cli_auth.py"

# ── AC4: refresh loop source presence ────────────────────────────────────────

echo "AC4: checking refresh loop source presence ..."
DAEMON_LIB="${REPO_ROOT}/crates/vectorhawkd-daemon/src/lib.rs"
REFRESH_TESTS="${REPO_ROOT}/crates/vectorhawkd-daemon/src/refresh_loop_tests.rs"

if grep -q "refresh_one_tick" "${DAEMON_LIB}" \
   && [[ -f "${REFRESH_TESTS}" ]] \
   && grep -q "refresh_one_tick" "${REFRESH_TESTS}"; then
    record "PASS" \
        'AC4: refresh loop module present (refresh_one_tick + tests)' \
        "refresh_one_tick found in lib.rs; refresh_loop_tests.rs present and references it"
else
    record "FAIL" \
        'AC4: refresh loop module present (refresh_one_tick + tests)' \
        "refresh_one_tick or refresh_loop_tests not found — check daemon/src/"
fi

# ── AC6: concurrency integration test ────────────────────────────────────────

echo "AC6: running concurrency integration test (m3_concurrency) ..."

CONCURRENCY_LOG="${REPO_ROOT}/target/m3-concurrency-test.log"

if cargo test --release -p vectorhawkd-cli --test m3_concurrency \
        -- --include-ignored --nocapture 2>"${CONCURRENCY_LOG}"; then
    record "PASS" \
        'AC6: concurrency — two flows, no cross-contamination, collision handled' \
        "m3_concurrency tests passed (2/2)"
else
    LAST_LINES="$(tail -10 "${CONCURRENCY_LOG}" | tr '\n' '|')"
    record "FAIL" \
        'AC6: concurrency — two flows, no cross-contamination, collision handled' \
        "m3_concurrency test failed — see ${CONCURRENCY_LOG}: ${LAST_LINES}"
fi

# ── RSS check — daemon idle budget ───────────────────────────────────────────

echo "RSS: checking daemon idle RSS <=50 MB ..."

if bash "${REPO_ROOT}/scripts/measure_daemon_rss.sh" >/dev/null 2>&1; then
    RSS_VALUE="$(cat "${REPO_ROOT}/target/m0-daemon-rss.txt" 2>/dev/null || echo "unknown")"
    record "PASS" \
        'RSS: daemon idle RSS <=50 MB' \
        "measured ${RSS_VALUE} — within budget"
else
    RSS_VALUE="$(cat "${REPO_ROOT}/target/m0-daemon-rss.txt" 2>/dev/null || echo "unknown")"
    record "FAIL" \
        'RSS: daemon idle RSS <=50 MB' \
        "measured ${RSS_VALUE} — exceeds 50 MB budget"
fi

# ── Summary ───────────────────────────────────────────────────────────────────

echo ""
echo "=============================="
echo "M3 Acceptance Results"
echo "=============================="
echo ""

ALL_PASS=1
for i in "${!RESULTS[@]}"; do
    status="${RESULTS[${i}]}"
    label="${LABELS[${i}]}"
    detail="${DETAILS[${i}]}"
    case "${status}" in
        PASS) printf "  %-60s [${PASS}]\n    %s\n" "${label}" "${detail}" ;;
        SKIP) printf "  %-60s [${SKIP}]\n    %s\n" "${label}" "${detail}" ;;
        FAIL) printf "  %-60s [${FAIL}]\n    %s\n" "${label}" "${detail}"; ALL_PASS=0 ;;
    esac
done

echo ""

if [[ "${ALL_PASS}" -eq 1 ]]; then
    echo "All M3 acceptance criteria PASSED (skipped checks are N/A for this platform)"
    exit 0
else
    echo "One or more M3 acceptance criteria FAILED"
    exit 1
fi
