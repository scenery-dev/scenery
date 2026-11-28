#!/bin/sh
# Tests for scripts/stamp-release-scripts.sh against the real scan.sh,
# install.sh and scan-mdm.sh and fake tarballs. Run: sh scripts/test_stamp.sh
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
STAMP="${HERE}/stamp-release-scripts.sh"
ROOT="$(mktemp -d)"
trap 'rm -rf "${ROOT}"' EXIT
FAILS=0
check() { if [ "$2" = "0" ]; then echo "ok   - $1"; else echo "FAIL - $1"; FAILS=$((FAILS+1)); fi; }
sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi; }
TRIPLES="aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu"

# release <dir> <version>: fake tarballs for every triple.
release() {
    rm -rf "$1"; mkdir -p "$1"
    for t in ${TRIPLES}; do echo "tarball $2 $t" > "$1/scenery-$2-$t.tar.gz"; done
}
sums() { ( cd "$1" && for f in *.tar.gz scan.sh install.sh scan-mdm.sh; do printf '%s  %s\n' "$(sha "$f")" "$f"; done ) > "$1/SHA256SUMS"; }
val() { sed -n "s/^$2=\"\(.*\)\"\$/\1/p" "$1"; }

# 0. The repo copies are unstamped: every STAMP_ value is empty.
for s in scan.sh install.sh scan-mdm.sh; do
    n="$(grep -c '^STAMP_[A-Za-z0-9_]*=""$' "${HERE}/$s")"
    [ "${n}" -ge 2 ] && ! grep -q '^STAMP_[A-Za-z0-9_]*="[^"]' "${HERE}/$s"
    check "repo $s is unstamped" $?
done

# 1. Stamp a release.
R="${ROOT}/r1"; release "${R}" 9.9.9
sh "${STAMP}" v9.9.9 "${HERE}" "${R}" >/dev/null; check "stamp exits 0" $?
for s in scan.sh install.sh; do
    [ "$(val "${R}/$s" STAMP_TAG)" = v9.9.9 ]; check "$s: STAMP_TAG is the tag" $?
    ok=0
    for t in ${TRIPLES}; do
        k="$(echo "$t" | tr - _)"
        [ "$(val "${R}/$s" "STAMP_SHA256_$k")" = "$(sha "${R}/scenery-9.9.9-$t.tar.gz")" ] || ok=1
    done
    check "$s: every tarball hash pinned" "${ok}"
    # Only the stamp lines differ from the repo copy.
    d="$(diff "${HERE}/$s" "${R}/$s" | grep '^[<>]' | grep -cv '^[<>] STAMP_')"
    [ "${d}" = 0 ]; check "$s: nothing but the stamp changed" $?
done
[ "$(val "${R}/scan-mdm.sh" STAMP_TAG)" = v9.9.9 ]; check "scan-mdm.sh: STAMP_TAG is the tag" $?
[ "$(val "${R}/scan-mdm.sh" STAMP_SCAN_SHA256)" = "$(sha "${R}/scan.sh")" ]
check "scan-mdm.sh pins the STAMPED scan.sh" $?
[ "$(val "${R}/scan-mdm.sh" STAMP_SCAN_SHA256)" != "$(sha "${HERE}/scan.sh")" ]
check "scan-mdm.sh does not pin the unstamped scan.sh" $?
for s in scan.sh install.sh scan-mdm.sh; do sh -n "${R}/$s"; check "stamped $s parses" $?; done

# 2. --check.
sums "${R}"
sh "${STAMP}" --check v9.9.9 "${R}" >/dev/null 2>&1; check "--check accepts a clean stamp" $?
rm -f "${R}"/*.tar.gz
sh "${STAMP}" --check v9.9.9 "${R}" >/dev/null 2>&1; check "--check needs only SHA256SUMS, not the tarballs" $?
sh "${STAMP}" --check v9.9.8 "${R}" >/dev/null 2>&1; [ $? != 0 ]; check "--check rejects the wrong tag" $?

R2="${ROOT}/r2"; release "${R2}" 9.9.9; sh "${STAMP}" v9.9.9 "${HERE}" "${R2}" >/dev/null
sed 's/^STAMP_TAG="v9.9.9"$/STAMP_TAG="v9.9.7"/' "${R2}/install.sh" > "${R2}/i" && mv "${R2}/i" "${R2}/install.sh"; sums "${R2}"
out="$(sh "${STAMP}" --check v9.9.9 "${R2}" 2>&1)"; [ $? != 0 ] && printf '%s' "${out}" | grep -q 'install.sh is not stamped for v9.9.9'
check "--check rejects a wrong stamp value even with matching SHA256SUMS" $?

R3="${ROOT}/r3"; release "${R3}" 9.9.9; sh "${STAMP}" v9.9.9 "${HERE}" "${R3}" >/dev/null; sums "${R3}"
echo tampered > "${R3}/scenery-9.9.9-x86_64-apple-darwin.tar.gz"
( cd "${R3}" && awk '$2!~/x86_64-apple-darwin/' SHA256SUMS > s && printf '%s  %s\n' "$(sha scenery-9.9.9-x86_64-apple-darwin.tar.gz)" scenery-9.9.9-x86_64-apple-darwin.tar.gz >> s && mv s SHA256SUMS )
sh "${STAMP}" --check v9.9.9 "${R3}" >/dev/null 2>&1; [ $? != 0 ]; check "--check rejects a pin that differs from SHA256SUMS" $?

R4="${ROOT}/r4"; release "${R4}" 9.9.9; sh "${STAMP}" v9.9.9 "${HERE}" "${R4}" >/dev/null; sums "${R4}"
echo x >> "${R4}/scan.sh"
sh "${STAMP}" --check v9.9.9 "${R4}" >/dev/null 2>&1; [ $? != 0 ]; check "--check rejects a script not matching SHA256SUMS" $?

R5="${ROOT}/r5"; mkdir -p "${R5}"; cp "${HERE}/scan.sh" "${HERE}/install.sh" "${HERE}/scan-mdm.sh" "${R5}/"
( cd "${R5}" && for f in scan.sh install.sh scan-mdm.sh; do printf '%s  %s\n' "$(sha "$f")" "$f"; done ) > "${R5}/SHA256SUMS"
sh "${STAMP}" --check v9.9.9 "${R5}" >/dev/null 2>&1; [ $? != 0 ]; check "--check rejects unstamped scripts" $?

# 3. Stamping refusals.
R6="${ROOT}/r6"; release "${R6}" 9.9.9; rm "${R6}/scenery-9.9.9-x86_64-unknown-linux-gnu.tar.gz"
out="$(sh "${STAMP}" v9.9.9 "${HERE}" "${R6}" 2>&1)"; [ $? != 0 ] && printf '%s' "${out}" | grep -q 'no tarball for x86_64_unknown_linux_gnu'
check "stamp refuses when a tarball is missing" $?
R7="${ROOT}/r7"; release "${R7}" 9.9.9
sh "${STAMP}" v9.9.9 "${R}" "${R7}" >/dev/null 2>&1; [ $? != 0 ]; check "stamp refuses already-stamped sources" $?
sh "${STAMP}" 'v9.9.9;x' "${HERE}" "${R7}" >/dev/null 2>&1; [ $? != 0 ]; check "stamp refuses a malformed tag" $?
S8="${ROOT}/s8"; mkdir -p "${S8}"; cp "${HERE}/scan.sh" "${HERE}/install.sh" "${HERE}/scan-mdm.sh" "${S8}/"
sed '/^# <<< scenery release stamp <<<$/d' "${HERE}/install.sh" > "${S8}/install.sh"
sh "${STAMP}" v9.9.9 "${S8}" "${R7}" >/dev/null 2>&1; [ $? != 0 ]; check "stamp refuses an unterminated block" $?
sh "${STAMP}" v9.9.9 "${HERE}" >/dev/null 2>&1; [ $? = 2 ]; check "usage error exits 2" $?

[ "${FAILS}" = 0 ] && echo "all passed" || { echo "${FAILS} failed"; exit 1; }
