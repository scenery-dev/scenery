#!/bin/sh
# Tests for scripts/install.sh verification against a local fake release
# server, unstamped (repo copy) and stamped (release copy). Every run installs
# into a throwaway HOME with --no-setup --no-modify-path, so nothing on this
# machine changes. Run: sh scripts/test_install_sh.sh
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
INSTALL_SH="${HERE}/install.sh"
ROOT="$(mktemp -d)"
SRV_PID=""
cleanup() { [ -n "${SRV_PID}" ] && kill "${SRV_PID}" 2>/dev/null; rm -rf "${ROOT}"; }
trap cleanup EXIT

FAILS=0
check() { if [ "$2" = "0" ]; then echo "ok   - $1"; else echo "FAIL - $1"; FAILS=$((FAILS+1)); fi; }

OS="$(uname -s)"; ARCH="$(uname -m)"
case "${OS}:${ARCH}" in
    Darwin:arm64)  TRIPLE=aarch64-apple-darwin ;;
    Darwin:x86_64) TRIPLE=x86_64-apple-darwin ;;
    Linux:x86_64)  TRIPLE=x86_64-unknown-linux-gnu ;;
    *) echo "unsupported platform, skipping"; exit 0 ;;
esac
sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi; }

# Fake release v9.9.9: a "scenery" that prints its argv.
REL="${ROOT}/srv/v9.9.9"; PKG="${ROOT}/pkg/scenery-9.9.9-${TRIPLE}"
mkdir -p "${REL}" "${PKG}"
printf '#!/bin/sh\necho "FAKE scenery $*"\n' > "${PKG}/scenery"; chmod 755 "${PKG}/scenery"
TARBALL="scenery-9.9.9-${TRIPLE}.tar.gz"
tar -czf "${ROOT}/good.tgz" -C "${ROOT}/pkg" "scenery-9.9.9-${TRIPLE}"
cp "${ROOT}/good.tgz" "${REL}/${TARBALL}"
printf '%s  %s\n' "$(sha "${REL}/${TARBALL}")" "${TARBALL}" > "${REL}/SHA256SUMS"
printf 'stub-signature\n' > "${REL}/SHA256SUMS.minisig"

# Stamped install.sh for v9.9.9.
SREL="${ROOT}/stamp"; mkdir -p "${SREL}"
for t in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu; do
    if [ "${t}" = "${TRIPLE}" ]; then cp "${ROOT}/good.tgz" "${SREL}/scenery-9.9.9-${t}.tar.gz"
    else echo "other ${t}" > "${SREL}/scenery-9.9.9-${t}.tar.gz"; fi
done
sh "${HERE}/stamp-release-scripts.sh" v9.9.9 "${HERE}" "${SREL}" >/dev/null || { echo "stamping failed"; exit 1; }
STAMPED="${SREL}/install.sh"

PORT=$((20000 + ($$ + 7919) % 20000))
( cd "${ROOT}/srv" && exec python3 -m http.server "${PORT}" --bind 127.0.0.1 >/dev/null 2>&1 ) &
SRV_PID=$!
_i=0
until curl -fs -o /dev/null "http://127.0.0.1:${PORT}/v9.9.9/SHA256SUMS"; do
    _i=$((_i+1)); [ "${_i}" -gt 100 ] && { echo "server did not start"; exit 1; }
    sleep 0.1
done

# Controlled PATH: the tools install.sh needs, and no minisign/gh.
TOOLS="${ROOT}/tools"; mkdir -p "${TOOLS}"
for t in sh curl tar awk head tr find mktemp uname chmod rm mkdir cat sed grep cp mv ln basename dirname id \
         sha256sum shasum openssl gzip gunzip perl; do
    p="$(command -v "$t" 2>/dev/null)" && [ -x "${p}" ] && ln -sf "${p}" "${TOOLS}/${t}"
done
mkdir -p "${ROOT}/stub-ok" "${ROOT}/stub-bad"
printf '#!/bin/sh\nexit 0\n' > "${ROOT}/stub-ok/minisign"
printf '#!/bin/sh\nexit 1\n' > "${ROOT}/stub-bad/minisign"
chmod 755 "${ROOT}/stub-ok/minisign" "${ROOT}/stub-bad/minisign"

# run <stubdir|""> <script> [VAR=val ...]: install into a fresh HOME.
N=0
run() {
    _stub="$1"; _script="$2"; shift 2
    N=$((N+1)); H="${ROOT}/home${N}"; mkdir -p "${H}" "${ROOT}/tmp${N}"
    _path="${TOOLS}"; [ -n "${_stub}" ] && _path="${_stub}:${TOOLS}"
    env -i PATH="${_path}" HOME="${H}" TMPDIR="${ROOT}/tmp${N}" SHELL=/bin/sh \
        SCENERY_INSTALL_BASE_URL="http://127.0.0.1:${PORT}" "$@" \
        sh "${_script}" --no-setup --no-modify-path > "${ROOT}/out" 2>&1
    RC=$?; OUT="$(cat "${ROOT}/out")"
}
installed() { [ -x "${H}/.local/bin/scenery" ] && "${H}/.local/bin/scenery" x | grep -q 'FAKE scenery'; }
nothing()   { [ ! -e "${H}/.local/bin/scenery" ]; }

# 1. Unstamped repo copy: requires a signature check.
run "" "${INSTALL_SH}" SCENERY_VERSION=v9.9.9
[ "${RC}" != 0 ] && echo "${OUT}" | grep -q 'Cannot verify' && nothing
check "unstamped, no minisign/gh: refused, nothing installed" $?
run "${ROOT}/stub-ok" "${INSTALL_SH}" SCENERY_VERSION=v9.9.9
[ "${RC}" = 0 ] && installed; check "unstamped, minisign ok: installed" $?
run "${ROOT}/stub-bad" "${INSTALL_SH}" SCENERY_VERSION=v9.9.9
[ "${RC}" != 0 ] && nothing; check "unstamped, minisign fails: refused" $?

# 2. Stamped copy: the pinned hash suffices, signature checked when available.
run "" "${STAMPED}"
[ "${RC}" = 0 ] && installed && [ -L "${H}/.local/bin/scn" ]
check "stamped, no minisign/gh, no SCENERY_VERSION: installed the stamped tag" $?
run "${ROOT}/stub-bad" "${STAMPED}"
[ "${RC}" != 0 ] && nothing; check "stamped, minisign present and failing: refused" $?
run "${ROOT}/stub-ok" "${STAMPED}"
[ "${RC}" = 0 ] && installed; check "stamped, minisign ok: installed" $?
run "" "${STAMPED}" SCENERY_VERSION=v9.9.8
[ "${RC}" != 0 ] && echo "${OUT}" | grep -q 'Cannot verify' && nothing
check "stamped, other SCENERY_VERSION, no minisign/gh: refused" $?
printf 'tampered' >> "${REL}/${TARBALL}"
run "" "${STAMPED}"
[ "${RC}" != 0 ] && echo "${OUT}" | grep -q 'pinned in this installer' && nothing
check "stamped, tampered tarball, no minisign/gh: refused, nothing installed" $?

[ "${FAILS}" = 0 ] && echo "all passed" || { echo "${FAILS} failed"; exit 1; }
