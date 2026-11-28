#!/bin/sh
# Tests for scripts/scan.sh against a local fake release server.
# Signature verification is exercised through stub `minisign` binaries on a
# controlled PATH (no skip variable). Run: sh scripts/test_scan_sh.sh
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
SCAN_SH="${HERE}/scan.sh"
ROOT="$(mktemp -d)"
SRV_PID=""
cleanup() { [ -n "${SRV_PID}" ] && kill "${SRV_PID}" 2>/dev/null; rm -rf "${ROOT}"; }
trap cleanup EXIT

FAILS=0
check() { # name, exit-code (0 = pass)
    if [ "$2" = "0" ]; then echo "ok   - $1"; else echo "FAIL - $1"; FAILS=$((FAILS+1)); fi
}

OS="$(uname -s)"; ARCH="$(uname -m)"
case "${OS}:${ARCH}" in
    Darwin:arm64)  TRIPLE=aarch64-apple-darwin ;;
    Darwin:x86_64) TRIPLE=x86_64-apple-darwin ;;
    Linux:x86_64)  TRIPLE=x86_64-unknown-linux-gnu ;;
    *) echo "unsupported platform, skipping"; exit 0 ;;
esac
TARBALL="scenery-9.9.9-${TRIPLE}.tar.gz"

sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi; }

# Fake release: a "scenery" that prints its argv, packed and checksummed.
REL="${ROOT}/srv/v9.9.9"
mkdir -p "${REL}" "${ROOT}/pkg"
printf '#!/bin/sh\necho "ARGV: $*"\nexit ${FAKE_EXIT:-0}\n' > "${ROOT}/pkg/scenery"
chmod 755 "${ROOT}/pkg/scenery"
tar -czf "${REL}/${TARBALL}" -C "${ROOT}/pkg" scenery
printf '%s  %s\n' "$(sha "${REL}/${TARBALL}")" "${TARBALL}" > "${REL}/SHA256SUMS"
printf 'stub-signature\n' > "${REL}/SHA256SUMS.minisig"
cp "${REL}/${TARBALL}" "${ROOT}/good.tgz"

PORT=$((20000 + $$ % 20000))
( cd "${ROOT}/srv" && exec python3 -m http.server "${PORT}" --bind 127.0.0.1 >/dev/null 2>&1 ) &
SRV_PID=$!
_i=0
until curl -fs -o /dev/null "http://127.0.0.1:${PORT}/v9.9.9/SHA256SUMS"; do
    _i=$((_i+1)); [ "${_i}" -gt 100 ] && { echo "server did not start"; exit 1; }
    sleep 0.1
done

# Controlled PATH: symlinks to the tools scan.sh needs, and no minisign/gh.
TOOLS="${ROOT}/tools"; mkdir -p "${TOOLS}"
for t in sh curl tar awk head tr find mktemp uname chmod rm mkdir cat sed grep python3 env dirname \
         sha256sum shasum openssl cksum gzip gunzip xz bzip2 sleep sort ls; do
    p="$(command -v "$t" 2>/dev/null)" && [ -x "${p}" ] && ln -sf "${p}" "${TOOLS}/${t}"
done
mkdir -p "${ROOT}/stub-ok" "${ROOT}/stub-bad"
printf '#!/bin/sh\nexit 0\n' > "${ROOT}/stub-ok/minisign"
printf '#!/bin/sh\nexit 1\n' > "${ROOT}/stub-bad/minisign"
chmod 755 "${ROOT}/stub-ok/minisign" "${ROOT}/stub-bad/minisign"

HOME_DIR="${ROOT}/home"; TMP_DIR="${ROOT}/tmp"
mkdir -p "${HOME_DIR}" "${TMP_DIR}"
touch "${HOME_DIR}/.zshrc"
# Portable listing: names, then a checksum of each file's content and its mode.
snap() { (cd "${HOME_DIR}" && find . | sort | while read -r f; do
    if [ -f "$f" ]; then printf '%s %s\n' "$f" "$(cksum < "$f")"; else printf '%s\n' "$f"; fi
done; ls -ld "${HOME_DIR}/.zshrc" | awk '{print $1}'); }

# run STUBDIR [VAR=val ...] -- sh scan.sh args...: STUBDIR is "" for none.
run() {
    _stub="$1"; shift
    _path="${TOOLS}"; [ -n "${_stub}" ] && _path="${_stub}:${TOOLS}"
    env -i PATH="${_path}" HOME="${HOME_DIR}" TMPDIR="${TMP_DIR}" \
        SCENERY_SCAN_BASE_URL="http://127.0.0.1:${PORT}" \
        SCENERY_SCAN_VERSION=v9.9.9 "$@"
}
tmp_empty() { [ -z "$(ls -A "${TMP_DIR}")" ]; }

BEFORE="$(snap)"

# 1. Good tarball + passing minisign stub, no token: --login.
OUT="$(run "${ROOT}/stub-ok" sh "${SCAN_SH}" --dry-run 2>&1)"; RC=$?
check "good tarball + minisign ok exits 0" "${RC}"
echo "${OUT}" | grep -q 'ARGV: scan --once --login --dry-run'; check "no token passes --login" $?
tmp_empty; check "temp dir removed after login run" $?

# 2. With token: no --login.
OUT="$(run "${ROOT}/stub-ok" SCENERY_SCAN_TOKEN=scn_scan_x sh "${SCAN_SH}" 2>&1)"; RC=$?
check "token run exits 0" "${RC}"
echo "${OUT}" | grep -q 'ARGV: scan --once$'; check "token run omits --login" $?
echo "${OUT}" | grep -q 'scn_scan_x'; [ $? -ne 0 ]; check "token not in output or argv" $?
tmp_empty; check "temp dir removed after token run" $?
# 2b. --token-file in args: no --login either.
OUT="$(run "${ROOT}/stub-ok" sh "${SCAN_SH}" --token-file /x/tok 2>&1)"
echo "${OUT}" | grep -q 'ARGV: scan --once --token-file /x/tok$'; check "--token-file omits --login" $?

# 3. Scan exit code is propagated.
run "${ROOT}/stub-ok" FAKE_EXIT=3 sh "${SCAN_SH}" >/dev/null 2>&1; RC=$?
[ "${RC}" = "3" ]; check "scan exit code propagated" $?

# 4. minisign stub fails: refuse, binary never runs.
OUT="$(run "${ROOT}/stub-bad" sh "${SCAN_SH}" 2>&1)"; RC=$?
[ "${RC}" != "0" ]; check "failing minisign exits non-zero" $?
echo "${OUT}" | grep -q 'refusing'; check "failing minisign says refusing" $?
echo "${OUT}" | grep -q 'ARGV'; [ $? -ne 0 ]; check "failing minisign: binary never ran" $?
tmp_empty; check "temp dir removed after minisign failure" $?

# 5. Neither minisign nor gh: refuse.
OUT="$(run "" sh "${SCAN_SH}" 2>&1)"; RC=$?
[ "${RC}" != "0" ]; check "no verifier exits non-zero" $?
echo "${OUT}" | grep -qi 'cannot verify'; check "no verifier is refused with a message" $?
echo "${OUT}" | grep -q 'ARGV'; [ $? -ne 0 ]; check "no verifier: binary never ran" $?

# 6. Bad version string.
OUT="$(run "${ROOT}/stub-ok" SCENERY_SCAN_VERSION='../evil' sh "${SCAN_SH}" 2>&1)"; RC=$?
[ "${RC}" != "0" ]; check "malformed SCENERY_SCAN_VERSION rejected" $?

# 7. Tampered tarball (minisign passes): checksum refuses.
printf 'tampered' >> "${REL}/${TARBALL}"
OUT="$(run "${ROOT}/stub-ok" sh "${SCAN_SH}" 2>&1)"; RC=$?
[ "${RC}" != "0" ]; check "tampered tarball exits non-zero" $?
echo "${OUT}" | grep -q 'refusing'; check "tampered tarball says refusing" $?
echo "${OUT}" | grep -q 'ARGV'; [ $? -ne 0 ]; check "tampered binary never ran" $?
tmp_empty; check "temp dir removed after refusal" $?

# 9. Stamped copy (what a release publishes): the tarball hash is pinned in
# the script, so it runs with no minisign or gh on PATH.
cp "${ROOT}/good.tgz" "${REL}/${TARBALL}"
SREL="${ROOT}/stamp"; mkdir -p "${SREL}"
for t in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu; do
    if [ "${t}" = "${TRIPLE}" ]; then cp "${ROOT}/good.tgz" "${SREL}/scenery-9.9.9-${t}.tar.gz"
    else echo "other ${t}" > "${SREL}/scenery-9.9.9-${t}.tar.gz"; fi
done
sh "${HERE}/stamp-release-scripts.sh" v9.9.9 "${HERE}" "${SREL}" >/dev/null || { echo "stamping failed"; exit 1; }
STAMPED="${SREL}/scan.sh"
mkdir -p "${ROOT}/gh-noauth" "${ROOT}/gh-bad"
printf '#!/bin/sh
exit 1
' > "${ROOT}/gh-noauth/gh"
printf '#!/bin/sh
[ "$1" = auth ] && exit 0
exit 1
' > "${ROOT}/gh-bad/gh"
chmod 755 "${ROOT}/gh-noauth/gh" "${ROOT}/gh-bad/gh"

OUT="$(run "" sh "${STAMPED}" --dry-run 2>&1)"; RC=$?
check "stamped, no minisign/gh: good tarball exits 0" "${RC}"
echo "${OUT}" | grep -q 'ARGV: scan --once --login --dry-run'; check "stamped, no minisign/gh: scan ran" $?
tmp_empty; check "stamped: temp dir removed" $?
OUT="$(env -i PATH="${TOOLS}" HOME="${HOME_DIR}" TMPDIR="${TMP_DIR}" SCENERY_SCAN_BASE_URL="http://127.0.0.1:${PORT}" \
    sh "${STAMPED}" --dry-run 2>&1)"; RC=$?
[ "${RC}" = 0 ] && echo "${OUT}" | grep -q 'ARGV:'; check "stamped, no SCENERY_SCAN_VERSION: runs the stamped tag" $?
OUT="$(run "${ROOT}/gh-noauth" sh "${STAMPED}" 2>&1)"; RC=$?
[ "${RC}" = 0 ] && echo "${OUT}" | grep -q 'ARGV:'; check "stamped, gh not signed in: pin alone, scan ran" $?
OUT="$(run "${ROOT}/gh-bad" sh "${STAMPED}" 2>&1)"; RC=$?
[ "${RC}" != 0 ] && ! echo "${OUT}" | grep -q 'ARGV'; check "stamped, signed-in gh fails attestation: refused" $?
OUT="$(run "${ROOT}/stub-bad" sh "${STAMPED}" 2>&1)"; RC=$?
[ "${RC}" != 0 ] && ! echo "${OUT}" | grep -q 'ARGV'; check "stamped, minisign present and failing: refused" $?
OUT="$(run "${ROOT}/stub-ok" sh "${STAMPED}" 2>&1)"; RC=$?
[ "${RC}" = 0 ] && echo "${OUT}" | grep -q 'ARGV:'; check "stamped, minisign ok: scan ran" $?
OUT="$(run "" SCENERY_SCAN_VERSION=v9.9.8 sh "${STAMPED}" 2>&1)"; RC=$?
[ "${RC}" != 0 ] && echo "${OUT}" | grep -qi 'cannot verify'; check "stamped, other tag, no minisign/gh: refused" $?
printf 'tampered' >> "${REL}/${TARBALL}"
OUT="$(run "" sh "${STAMPED}" 2>&1)"; RC=$?
[ "${RC}" != 0 ]; check "stamped, tampered tarball, no minisign/gh: exits non-zero" $?
echo "${OUT}" | grep -q 'pinned in scan.sh — refusing'; check "stamped, tampered tarball: says pinned mismatch" $?
echo "${OUT}" | grep -q 'ARGV'; [ $? -ne 0 ]; check "stamped, tampered binary never ran" $?
tmp_empty; check "stamped: temp dir removed after refusal" $?

# 8. HOME untouched.
AFTER="$(snap)"
[ "${BEFORE}" = "${AFTER}" ]; check "HOME unchanged" $?

[ "${FAILS}" = "0" ] && echo "all passed" || { echo "${FAILS} failed"; exit 1; }
