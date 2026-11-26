#!/bin/sh
# Tests for scripts/scan-mdm.sh with stub stat/loginctl/launchctl/sudo/id/curl
# on a controlled PATH. Run: sh scripts/test_scan_mdm.sh
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
MDM="${HERE}/scan-mdm.sh"
ROOT="$(mktemp -d)"
trap 'rm -rf "${ROOT}"' EXIT
FAILS=0
check() { if [ "$2" = "0" ]; then echo "ok   - $1"; else echo "FAIL - $1"; FAILS=$((FAILS+1)); fi; }

TOKEN="sct_SECRETVALUE123"
BIN="${ROOT}/bin"; LOG="${ROOT}/log"; mkdir -p "${BIN}"
for t in sh awk sed head mktemp chmod rm cat printf read grep; do
    p="$(command -v "$t" 2>/dev/null)" && [ -x "${p}" ] && ln -sf "${p}" "${BIN}/${t}"
done
stub() { printf '%s\n' "$2" > "${BIN}/$1"; chmod 755 "${BIN}/$1"; }

stub uname 'echo "${FAKE_OS:-Darwin}"'
stub stat 'echo "${FAKE_CONSOLE:-}"'
stub id 'echo 501'
stub loginctl '
case "$1" in
  list-sessions) [ -n "${FAKE_CONSOLE:-}" ] && echo "c1 1000 ${FAKE_CONSOLE} seat0" ;;
  show-session) printf "Name=%s\nActive=yes\nClass=user\nType=x11\n" "${FAKE_CONSOLE}" ;;
esac'
# Every stub records its full argv, one line per call.
stub curl '
echo "curl $*" >> "${LOG}"
while [ $# -gt 0 ]; do [ "$1" = "-o" ] && out="$2"; shift; done
cat > "${out}" <<SCAN
echo "SCAN_RAN user=\$(echo \${FAKE_AS:-?}) token_in_env=\${SCENERY_SCAN_TOKEN:+yes} args=\$*" >> "${LOG}"
exit \${FAKE_SCAN_EXIT:-0}
SCAN'
stub launchctl 'echo "launchctl $*" >> "${LOG}"; shift 2; exec "$@"'
stub sudo '
echo "sudo $*" >> "${LOG}"
while [ $# -gt 0 ]; do
  case "$1" in
    --preserve-env=*|-H) shift ;;
    -u) FAKE_AS="$2"; export FAKE_AS; shift 2 ;;
    *) break ;;
  esac
done
exec "$@"'

run() { : > "${LOG}"; env -i PATH="${BIN}" LOG="${LOG}" "$@" /bin/sh "${MDM}" ${MDM_ARGS:-}; }
export LOG

# 1. No token: exit 2.
run FAKE_CONSOLE=alice >/dev/null 2>&1; rc=$?
[ "${rc}" = 2 ]; check "missing token exits 2" $?

for os in Darwin Linux; do
    # 2. No console user (and root/loginwindow): exit 0, scan.sh never ran.
    for who in "" root loginwindow; do
        out="$(run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_OS=${os} FAKE_CONSOLE="${who}" 2>&1)"; rc=$?
        [ "${rc}" = 0 ] && ! grep -q SCAN_RAN "${LOG}" && ! grep -q '^curl' "${LOG}" \
            && printf '%s' "${out}" | grep -q 'no user logged in; nothing scanned'
        check "${os}: console user '${who}' -> exit 0, nothing scanned" $?
    done

    # 3. Console user alice: scan ran as alice, token in env, not in any argv.
    run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_OS=${os} FAKE_CONSOLE=alice >/dev/null 2>&1; rc=$?
    [ "${rc}" = 0 ] && grep -q 'SCAN_RAN user=alice token_in_env=yes' "${LOG}"
    check "${os}: scan.sh ran as alice with token in env" $?
    ! grep -q "${TOKEN}" "${LOG}"
    check "${os}: token absent from argv of curl, sudo, launchctl, sh" $?
    if [ "${os}" = Darwin ]; then
        grep -q '^launchctl asuser 501 sudo' "${LOG}"; check "Darwin: launchctl asuser 501" $?
    fi

    # 4. Scan exit code is returned (3 = upload failed).
    run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_OS=${os} FAKE_CONSOLE=alice FAKE_SCAN_EXIT=3 >/dev/null 2>&1; rc=$?
    [ "${rc}" = 3 ]; check "${os}: scan exit code 3 passed through" $?
done

# 5. Arguments pass through.
MDM_ARGS=--dry-run run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_CONSOLE=alice >/dev/null 2>&1
grep -q 'args=--dry-run' "${LOG}"; check "arguments pass through to scan.sh" $?

[ "${FAILS}" = 0 ] && echo "all passed" || { echo "${FAILS} failed"; exit 1; }
