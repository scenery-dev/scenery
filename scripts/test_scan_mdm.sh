#!/bin/sh
# Tests for scripts/scan-mdm.sh with stub stat/loginctl/launchctl/sudo/id/curl/
# minisign/gh
# on a controlled PATH. Run: sh scripts/test_scan_mdm.sh
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
MDM="${HERE}/scan-mdm.sh"
ROOT="$(mktemp -d)"
trap 'rm -rf "${ROOT}"' EXIT
FAILS=0
check() { if [ "$2" = "0" ]; then echo "ok   - $1"; else echo "FAIL - $1"; FAILS=$((FAILS+1)); fi; }

TOKEN="scn_scan_SECRETVALUE123"
BIN="${ROOT}/bin"; LOG="${ROOT}/log"; mkdir -p "${BIN}"
for t in sh awk sed head cut mktemp chmod rm cat printf read grep ls tr env cp sha256sum shasum openssl perl; do
    p="$(command -v "$t" 2>/dev/null)" && [ -x "${p}" ] && ln -sf "${p}" "${BIN}/${t}"
done
stub() { printf '%s\n' "$2" > "${BIN}/$1"; chmod 755 "${BIN}/$1"; }

stub uname 'echo "${FAKE_OS:-Darwin}"'
stub stat 'echo "${FAKE_CONSOLE:-}"'
stub id 'echo 501'
stub loginctl '
case "$1" in
  list-sessions) [ -n "${FAKE_CONSOLE:-}" ] && echo "c1 1000 ${FAKE_CONSOLE} seat0" ;;
  show-session) printf "Name=%s\nActive=yes\nClass=user\nType=%s\n" "${FAKE_CONSOLE}" "${FAKE_TYPE:-x11}" ;;
esac'
# The fake scan.sh records how it was run. SHA256SUMS lists its real hash.
FAKE_SCAN="${ROOT}/fake-scan.sh"
cat > "${FAKE_SCAN}" <<'SCAN'
tf=""; prev=""
for a in "$@"; do [ "$prev" = "--token-file" ] && tf="$a"; prev="$a"; done
echo "SCAN_RAN user=${FAKE_AS:-?} token_in_env=${SCENERY_SCAN_TOKEN:+yes} version=${SCENERY_SCAN_VERSION:-} registry=${SCENERY_REGISTRY_URL:-} args=$*" >> "${LOG}"
echo "TOKENFILE $tf" >> "${LOG}"
echo "TOKENMODE $(ls -l "$tf" | cut -c1-10) content=$(cat "$tf")" >> "${LOG}"
exit ${FAKE_SCAN_EXIT:-0}
SCAN
if command -v sha256sum >/dev/null 2>&1; then _sum="$(sha256sum "${FAKE_SCAN}" | awk '{print $1}')"
else _sum="$(shasum -a 256 "${FAKE_SCAN}" | awk '{print $1}')"; fi
SUMS="${ROOT}/SHA256SUMS"
printf '%s  %s\n%s  %s\n' "${_sum}" scan.sh 0000000000000000000000000000000000000000000000000000000000000000 scan-mdm.sh > "${SUMS}"
export FAKE_SCAN SUMS

# Every stub records its full argv, one line per call. curl serves the latest
# release lookup, scan.sh, SHA256SUMS and its signature by URL.
stub curl '
echo "curl $*" >> "${LOG}"
[ -n "${FAKE_CURL_FAIL:-}" ] && exit 22
out=""; url=""
while [ $# -gt 0 ]; do case "$1" in -o) out="$2"; shift ;; -*) ;; *) url="$1" ;; esac; shift; done
case "${url}" in *"${FAKE_CURL_FAIL_ON:-@none@}") exit 22 ;; esac
emit() { if [ -n "${out}" ]; then cat > "${out}"; else cat; fi; }
case "${url}" in
  */releases/latest) printf "{\"tag_name\": \"%s\"}\n" "${FAKE_LATEST:-v9.9.9}" | emit ;;
  */scan.sh) if [ -n "${FAKE_TAMPER:-}" ]; then { cat "${FAKE_SCAN}"; echo "# evil"; } | emit; else emit < "${FAKE_SCAN}"; fi ;;
  */SHA256SUMS) emit < "${SUMS}" ;;
  */SHA256SUMS.minisig) echo sig | emit ;;
  *) exit 22 ;;
esac'
stub minisign 'echo "minisign $*" >> "${LOG}"; exit "${FAKE_MINISIGN_RC:-0}"'
stub chown 'echo "chown $*" >> "${LOG}"'
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

run() { : > "${LOG}"; env -i PATH="${BIN}" LOG="${LOG}" FAKE_SCAN="${FAKE_SCAN}" SUMS="${SUMS}" "$@" /bin/sh "${MDM_FILE:-${MDM}}" ${MDM_ARGS:-}; }
export LOG

# 1. No token: exit 2.
run FAKE_CONSOLE=alice >/dev/null 2>&1; rc=$?
[ "${rc}" = 2 ]; check "missing token exits 2" $?

for os in Darwin Linux; do
    # 2. No console user (and root/loginwindow): exit 0, scan.sh never ran.
    for who in "" root loginwindow _mbsetupuser; do
        out="$(run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_OS=${os} FAKE_CONSOLE="${who}" 2>&1)"; rc=$?
        [ "${rc}" = 0 ] && ! grep -q SCAN_RAN "${LOG}" && ! grep -q '^curl' "${LOG}" \
            && printf '%s' "${out}" | grep -q 'no user logged in; nothing scanned'
        check "${os}: console user '${who}' -> exit 0, nothing scanned" $?
    done

    # 3. Console user alice: scan ran as alice, token in env, not in any argv.
    run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_OS=${os} FAKE_CONSOLE=alice >/dev/null 2>&1; rc=$?
    [ "${rc}" = 0 ] && grep -q 'SCAN_RAN user=alice token_in_env= ' "${LOG}"
    check "${os}: scan.sh ran as alice, token not in env" $?
    grep -q '^curl .*https://github.com/scenery-dev/scenery/releases/download/v9.9.9/scan.sh$' "${LOG}" \
        && ! grep -q 'install.scenery.dev' "${LOG}"
    check "${os}: scan.sh fetched from the latest release tag, not install.scenery.dev" $?
    grep -q '^minisign -Vm .*/SHA256SUMS -x .*/SHA256SUMS.minisig -P RWREIYd8cL30cy+6WHSsE+0oVDsfoPu/1BmCGX4rb0Fgs4+ZC3jjW/gp' "${LOG}"
    check "${os}: SHA256SUMS verified with the release minisign key" $?
    grep -q 'SCAN_RAN .* version=v9.9.9 registry= ' "${LOG}"
    check "${os}: scan.sh runs the same tag (SCENERY_SCAN_VERSION forwarded)" $?
    ! grep -v '^TOKENMODE' "${LOG}" | grep -q "${TOKEN}"
    check "${os}: token absent from argv of curl, sudo, launchctl, sh" $?
    grep -q 'args=--token-file /var/tmp/scenery-scan-mdm\.' "${LOG}" && ! grep -q -- '--login' "${LOG}"
    check "${os}: args carry --token-file, not --login" $?
    grep -q "TOKENMODE -rw------- content=${TOKEN}" "${LOG}"
    check "${os}: token file is 0600 with the token" $?
    grep -q '^chown alice /var/tmp/scenery-scan-mdm\.' "${LOG}"
    check "${os}: token file chowned to alice" $?
    tf="$(sed -n 's/^TOKENFILE //p' "${LOG}")"; [ -n "${tf}" ] && [ ! -e "${tf}" ] && [ ! -e "$(dirname "${tf}")" ]
    check "${os}: token file and work dir removed afterwards" $?
    if [ "${os}" = Darwin ]; then
        grep -q '^launchctl asuser 501 sudo' "${LOG}"; check "Darwin: launchctl asuser 501" $?
    fi

    # 4. Scan exit code is returned (3 = upload failed).
    run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_OS=${os} FAKE_CONSOLE=alice FAKE_SCAN_EXIT=3 >/dev/null 2>&1; rc=$?
    [ "${rc}" = 3 ]; check "${os}: scan exit code 3 passed through" $?
done

# Headless Linux (tty session): exit 0, nothing scanned.
out="$(run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_OS=Linux FAKE_CONSOLE=alice FAKE_TYPE=tty 2>&1)"; rc=$?
[ "${rc}" = 0 ] && ! grep -q SCAN_RAN "${LOG}"; check "Linux: non-graphical session -> exit 0, nothing scanned" $?

# Download failure: exit 1, no scan, no leftovers.
run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_CONSOLE=alice FAKE_CURL_FAIL=1 >/dev/null 2>&1; rc=$?
[ "${rc}" = 1 ] && ! grep -q SCAN_RAN "${LOG}"; check "download failure exits 1" $?
[ -z "$(ls -d /var/tmp/scenery-scan-mdm.* 2>/dev/null)" ]; check "no work dir left in /var/tmp" $?

# 5. Arguments pass through.
MDM_ARGS=--dry-run run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_CONSOLE=alice >/dev/null 2>&1
grep -q 'args=--token-file .* --dry-run' "${LOG}"; check "arguments pass through to scan.sh" $?

# 6. Verification failures refuse: nothing runs, exit 1, no leftovers.
refuses() {  # <label> <env...>
    _label="$1"; shift
    out="$(run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_CONSOLE=alice "$@" 2>&1)"; rc=$?
    [ "${rc}" = 1 ] && ! grep -q SCAN_RAN "${LOG}" && ! grep -q '^chown' "${LOG}" \
        && printf '%s' "${out}" | grep -q 'refusing to run'
    check "${_label}: refused, scan.sh never ran" $?
    [ -z "$(ls -d /var/tmp/scenery-scan-mdm.* 2>/dev/null)" ]; check "${_label}: no work dir left" $?
}
refuses "minisign verification fails" FAKE_MINISIGN_RC=1
refuses "tampered scan.sh" FAKE_TAMPER=1
refuses "SHA256SUMS missing" FAKE_CURL_FAIL_ON=/SHA256SUMS
refuses "signature missing" FAKE_CURL_FAIL_ON=/SHA256SUMS.minisig
mv "${BIN}/minisign" "${ROOT}/minisign.off"
refuses "neither minisign nor gh" X=1
stub gh 'echo "gh $*" >> "${LOG}"; exit "${FAKE_GH_RC:-0}"'
refuses "gh attestation fails" FAKE_GH_RC=1
run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_CONSOLE=alice >/dev/null 2>&1; rc=$?
[ "${rc}" = 0 ] && grep -q '^gh attestation verify .*/SHA256SUMS --repo scenery-dev/scenery' "${LOG}" && grep -q SCAN_RAN "${LOG}"
check "gh attestation fallback verifies SHA256SUMS" $?
rm -f "${BIN}/gh"; mv "${ROOT}/minisign.off" "${BIN}/minisign"

# 7. SCENERY_SCAN_VERSION pins the tag; no latest lookup.
run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_CONSOLE=alice SCENERY_SCAN_VERSION=v2.8.1 >/dev/null 2>&1; rc=$?
[ "${rc}" = 0 ] && grep -q '/releases/download/v2.8.1/scan.sh$' "${LOG}" && ! grep -q 'releases/latest' "${LOG}" \
    && grep -q 'SCAN_RAN .* version=v2.8.1 ' "${LOG}"
check "SCENERY_SCAN_VERSION pins scan.sh and the scan to that tag" $?
out="$(run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_CONSOLE=alice 'SCENERY_SCAN_VERSION=v2.8.0;x' 2>&1)"; rc=$?
[ "${rc}" = 1 ] && ! grep -q '^curl' "${LOG}"; check "malformed SCENERY_SCAN_VERSION refused before any download" $?

# 8. SCENERY_REGISTRY_URL survives sudo -H (forwarded explicitly).
run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_CONSOLE=alice SCENERY_REGISTRY_URL=https://dev.scenery.dev >/dev/null 2>&1
grep -q 'SCAN_RAN .* registry=https://dev.scenery.dev ' "${LOG}"; check "SCENERY_REGISTRY_URL forwarded to scan.sh" $?

# 9. Stamped wrapper (what a release publishes): pins scan.sh's hash, so it
# runs with no minisign or gh on PATH.
MDM_FILE="${ROOT}/scan-mdm-stamped.sh"
sed -e 's/^STAMP_TAG=""$/STAMP_TAG="v9.9.9"/' -e "s/^STAMP_SCAN_SHA256=\"\"\$/STAMP_SCAN_SHA256=\"${_sum}\"/" "${MDM}" > "${MDM_FILE}"
grep -q "^STAMP_SCAN_SHA256=\"${_sum}\"$" "${MDM_FILE}"; check "stamped fixture has the pin" $?
mv "${BIN}/minisign" "${ROOT}/minisign.off"
run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_CONSOLE=alice >/dev/null 2>&1; rc=$?
[ "${rc}" = 0 ] && grep -q 'SCAN_RAN user=alice .* version=v9.9.9 ' "${LOG}"
check "stamped, no minisign/gh: scan.sh verified by pin and ran" $?
! grep -q 'releases/latest' "${LOG}" && ! grep -q 'SHA256SUMS' "${LOG}"
check "stamped: no latest lookup, no SHA256SUMS needed" $?
refuses "stamped, tampered scan.sh" FAKE_TAMPER=1
refuses "stamped, other tag without minisign/gh" SCENERY_SCAN_VERSION=v2.8.1
stub gh 'echo "gh $*" >> "${LOG}"; exit 1'
run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_CONSOLE=alice >/dev/null 2>&1; rc=$?
[ "${rc}" = 0 ] && grep -q SCAN_RAN "${LOG}" && ! grep -q 'gh attestation' "${LOG}"
check "stamped, gh not signed in: pin alone, scan ran" $?
rm -f "${BIN}/gh"; mv "${ROOT}/minisign.off" "${BIN}/minisign"
refuses "stamped, minisign present and failing" FAKE_MINISIGN_RC=1
run SCENERY_SCAN_TOKEN="${TOKEN}" FAKE_CONSOLE=alice >/dev/null 2>&1; rc=$?
[ "${rc}" = 0 ] && grep -q '^minisign -Vm' "${LOG}" && grep -q SCAN_RAN "${LOG}"
check "stamped, minisign present: also checks the signature" $?
MDM_FILE=""

[ "${FAILS}" = 0 ] && echo "all passed" || { echo "${FAILS} failed"; exit 1; }
