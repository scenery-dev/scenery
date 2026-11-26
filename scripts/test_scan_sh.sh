#!/bin/sh
# Tests for scripts/scan.sh against a local fake release server.
# Run: sh scripts/test_scan_sh.sh
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
SCAN_SH="${HERE}/scan.sh"
ROOT="$(mktemp -d)"
SRV_PID=""
cleanup() { [ -n "${SRV_PID}" ] && kill "${SRV_PID}" 2>/dev/null; rm -rf "${ROOT}"; }
trap cleanup EXIT

FAILS=0
check() { # name, condition-exit-code
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

PORT=$((20000 + $$ % 20000))
( cd "${ROOT}/srv" && exec python3 -m http.server "${PORT}" --bind 127.0.0.1 >/dev/null 2>&1 ) &
SRV_PID=$!
sleep 1

HOME_DIR="${ROOT}/home"; TMP_DIR="${ROOT}/tmp"
mkdir -p "${HOME_DIR}" "${TMP_DIR}"
touch "${HOME_DIR}/.zshrc"
snap() { (cd "${HOME_DIR}" && find . -exec ls -ldT {} + 2>/dev/null || find . | sort); }

run() { # extra env assignments via "$@" before sh
    env -i PATH="${PATH}" HOME="${HOME_DIR}" TMPDIR="${TMP_DIR}" \
        SCENERY_SCAN_BASE_URL="http://127.0.0.1:${PORT}" \
        SCENERY_SCAN_VERSION=v9.9.9 SCENERY_INSTALL_SKIP_SIG=1 "$@"
}

BEFORE="$(snap)"

# 1. Good tarball, no token: --login.
OUT="$(run sh "${SCAN_SH}" --dry-run 2>&1)"; RC=$?
check "good tarball exits 0" "${RC}"
echo "${OUT}" | grep -q 'ARGV: scan --once --login --dry-run'; check "no token passes --login" $?
[ -z "$(ls -A "${TMP_DIR}")" ]; check "temp dir removed after login run" $?

# 2. With token: no --login.
OUT="$(run SCENERY_SCAN_TOKEN=scn_scan_x sh "${SCAN_SH}" 2>&1)"; RC=$?
check "token run exits 0" "${RC}"
echo "${OUT}" | grep -q 'ARGV: scan --once$'; check "token run omits --login" $?
echo "${OUT}" | grep -q 'scn_scan_x'; [ $? -ne 0 ]; check "token not in output or argv" $?
[ -z "$(ls -A "${TMP_DIR}")" ]; check "temp dir removed after token run" $?

# 3. Scan exit code is propagated.
run FAKE_EXIT=3 sh "${SCAN_SH}" >/dev/null 2>&1; RC=$?
[ "${RC}" = "3" ]; check "scan exit code propagated" $?

# 4. Tampered tarball: refuse.
printf 'tampered' >> "${REL}/${TARBALL}"
OUT="$(run sh "${SCAN_SH}" 2>&1)"; RC=$?
[ "${RC}" != "0" ]; check "tampered tarball exits non-zero" $?
echo "${OUT}" | grep -q 'refusing'; check "tampered tarball says refusing" $?
echo "${OUT}" | grep -q 'ARGV'; [ $? -ne 0 ]; check "tampered binary never ran" $?
[ -z "$(ls -A "${TMP_DIR}")" ]; check "temp dir removed after refusal" $?

# 5. HOME untouched.
AFTER="$(snap)"
[ "${BEFORE}" = "${AFTER}" ]; check "HOME unchanged" $?

[ "${FAILS}" = "0" ] && echo "all passed" || { echo "${FAILS} failed"; exit 1; }
