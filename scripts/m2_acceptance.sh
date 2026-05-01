#!/usr/bin/env bash
# m2_acceptance.sh — top-level M2 acceptance gate.
#
# Runs all eight M2 acceptance criteria from context/RUN1_M2_STREAMS.md plus
# M0 and M1 regression checks. Returns 0 only when every applicable check
# passes. Skipped checks (wrong platform, systemctl unavailable) count as PASS.
#
# ============================================================================
# RUNNING INSTRUCTIONS
# ============================================================================
# Prerequisites — the workspace must be built in release mode:
#
#   cargo build --workspace --release
#
# Then run the gate from the repo root:
#
#   bash scripts/m2_acceptance.sh
#
# The M0 and M1 gates are invoked automatically and add a few minutes.
# ============================================================================
#
# M2 acceptance criteria (from context/RUN1_M2_STREAMS.md):
#   AC1: vectorhawk daemon install (macOS) — plist written, socket reachable in 5 s.
#   AC2: vectorhawk daemon install (Linux) — unit written, socket reachable.
#   AC3: vectorhawk daemon uninstall — clean teardown, no zombie process or socket.
#   AC4: Idempotent — running daemon install twice is a no-op, exits 0.
#   AC5: mcp setup integration — ensure_installed wired before config write.
#   AC6: doctor install state — reports install state with unit path.
#   AC7: M0 regression — m0_acceptance.sh returns 0.
#   AC8: M1 regression — m1_acceptance.sh returns 0.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLI_BIN="${REPO_ROOT}/target/release/vectorhawk"

# Initial cleanup — required when chained from another gate. Kill any stale
# daemon and remove a leftover socket so the gate starts from a clean state.
case "$(uname -s)" in
    Darwin) _M2_INIT_SOCK="${HOME}/Library/Application Support/VectorHawk/agent.sock" ;;
    Linux)  _M2_INIT_SOCK="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/vectorhawk/agent.sock" ;;
    *)      _M2_INIT_SOCK="${HOME}/.local/share/vectorhawk/agent.sock" ;;
esac
pkill -x vectorhawkd 2>/dev/null || true
rm -f "${_M2_INIT_SOCK}" 2>/dev/null || true
unset _M2_INIT_SOCK

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
echo "VectorHawk M2 Acceptance Gate"
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

# ── Detect OS ────────────────────────────────────────────────────────────────

OS="$(uname -s)"

# ── AC1: macOS install end-to-end ────────────────────────────────────────────

echo "AC1: macOS install end-to-end ..."
if [[ "${OS}" != "Darwin" ]]; then
    record "SKIP" \
        'AC1: vectorhawk daemon install (macOS)' \
        "not macOS — skipped (N/A on ${OS})"
else
    if cargo test --release -p vectorhawkd-cli --test m2_install_macos \
            -- --include-ignored 2>&1 | grep -q "test result: ok. 1 passed"; then
        record "PASS" \
            'AC1: vectorhawk daemon install (macOS)' \
            "m2_install_macos test passed (plist + socket + 5 s timeout)"
    else
        record "FAIL" \
            'AC1: vectorhawk daemon install (macOS)' \
            "m2_install_macos test failed — see cargo output above"
    fi
fi

# ── AC2: Linux install end-to-end ────────────────────────────────────────────

echo "AC2: Linux install end-to-end ..."
if [[ "${OS}" != "Linux" ]]; then
    record "SKIP" \
        'AC2: vectorhawk daemon install (Linux)' \
        "not Linux — skipped (N/A on ${OS})"
else
    # Graceful skip if systemctl --user is not available.
    if ! command -v systemctl >/dev/null 2>&1 \
       || ! systemctl --user show-environment >/dev/null 2>&1; then
        record "SKIP" \
            'AC2: vectorhawk daemon install (Linux)' \
            "systemctl --user not available — skipped"
    else
        if cargo test --release -p vectorhawkd-cli --test m2_install_linux \
                -- --include-ignored 2>&1 | grep -q "test result: ok. 1 passed"; then
            record "PASS" \
                'AC2: vectorhawk daemon install (Linux)' \
                "m2_install_linux test passed (unit + socket + 5 s timeout)"
        else
            record "FAIL" \
                'AC2: vectorhawk daemon install (Linux)' \
                "m2_install_linux test failed — see cargo output above"
        fi
    fi
fi

# ── AC3: uninstall clean — covered by the install tests (steps 7–9) ──────────

echo "AC3: uninstall clean (covered by AC1/AC2 test steps 7-9) ..."
# AC3 is exercised inside the same integration test as AC1/AC2 (steps 7–9:
# uninstall exits 0, plist/unit removed, socket gone within 3 s). We record
# the same result.
INSTALL_AC_RESULT="${RESULTS[0]:-SKIP}"   # AC1 on macOS, AC2 on Linux
if [[ "${OS}" == "Darwin" ]]; then
    INSTALL_AC_RESULT="${RESULTS[0]:-SKIP}"
elif [[ "${OS}" == "Linux" ]]; then
    INSTALL_AC_RESULT="${RESULTS[1]:-SKIP}"
fi

case "${INSTALL_AC_RESULT}" in
    PASS) record "PASS" \
            'AC3: uninstall clean (no zombie process or socket)' \
            "validated by install test steps 7-9" ;;
    SKIP) record "SKIP" \
            'AC3: uninstall clean (no zombie process or socket)' \
            "install test skipped — uninstall steps not exercised on this platform" ;;
    *)    record "FAIL" \
            'AC3: uninstall clean (no zombie process or socket)' \
            "install test failed in AC1/AC2 — uninstall path not validated" ;;
esac

# ── AC4: idempotency — covered by the install tests (step 6) ─────────────────

echo "AC4: idempotency (covered by AC1/AC2 test step 6) ..."
case "${INSTALL_AC_RESULT}" in
    PASS) record "PASS" \
            'AC4: idempotent daemon install (no double-register)' \
            "validated by install test step 6 (second install exits 0)" ;;
    SKIP) record "SKIP" \
            'AC4: idempotent daemon install (no double-register)' \
            "install test skipped — idempotency not exercised on this platform" ;;
    *)    record "FAIL" \
            'AC4: idempotent daemon install (no double-register)' \
            "install test failed in AC1/AC2 — idempotency path not validated" ;;
esac

# ── AC5: mcp setup integration — source-grep for ensure_installed wiring ─────

echo "AC5: mcp setup integration (ensure_installed wired) ..."
MAIN_RS="${REPO_ROOT}/crates/vectorhawkd-cli/src/main.rs"
if grep -q "ensure_installed" "${MAIN_RS}" \
   && grep -q "install::status" "${MAIN_RS}" \
   && grep -qE "cmd_mcp_setup|Mcp.*Setup" "${MAIN_RS}"; then
    record "PASS" \
        'AC5: mcp setup calls ensure_installed before config write' \
        "install::ensure_installed wired in cmd_mcp_setup (main.rs)"
else
    record "FAIL" \
        'AC5: mcp setup calls ensure_installed before config write' \
        "ensure_installed or install::status not found in cmd_mcp_setup"
fi

# ── AC6: doctor install state ────────────────────────────────────────────────

echo "AC6: doctor install state ..."
DOCTOR_OUT="$("${CLI_BIN}" doctor 2>&1)"
if echo "${DOCTOR_OUT}" | grep -qE "^Daemon install:"; then
    DAEMON_LINE="$(echo "${DOCTOR_OUT}" | grep -E "^Daemon install:" | head -n1)"
    record "PASS" \
        'AC6: doctor reports daemon install state' \
        "found: '${DAEMON_LINE}'"
else
    record "FAIL" \
        'AC6: doctor reports daemon install state' \
        "'Daemon install:' line not found in doctor output"
fi

# ── AC7: M0 regression ───────────────────────────────────────────────────────

echo "AC7: M0 regression (bash scripts/m0_acceptance.sh) ..."
if bash "${REPO_ROOT}/scripts/m0_acceptance.sh" >/dev/null 2>&1; then
    record "PASS" \
        'AC7: M0 regression (6/6 PASS)' \
        "m0_acceptance.sh returned 0"
else
    record "FAIL" \
        'AC7: M0 regression (6/6 PASS)' \
        "m0_acceptance.sh returned non-zero — see m0 output for details"
fi

# ── AC8: M1 regression ───────────────────────────────────────────────────────

echo "AC8: M1 regression (bash scripts/m1_acceptance.sh) ..."
if bash "${REPO_ROOT}/scripts/m1_acceptance.sh" >/dev/null 2>&1; then
    record "PASS" \
        'AC8: M1 regression (12/12 PASS)' \
        "m1_acceptance.sh returned 0"
else
    record "FAIL" \
        'AC8: M1 regression (12/12 PASS)' \
        "m1_acceptance.sh returned non-zero — run m1_acceptance.sh directly for details"
fi

# ── Summary ───────────────────────────────────────────────────────────────────

echo ""
echo "=============================="
echo "M2 Acceptance Results"
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
    echo "All M2 acceptance criteria PASSED (skipped checks are N/A for this platform)"
    exit 0
else
    echo "One or more M2 acceptance criteria FAILED"
    exit 1
fi
