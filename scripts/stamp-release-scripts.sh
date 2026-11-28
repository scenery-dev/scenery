#!/bin/sh
# stamp-release-scripts.sh: pin a release's checksums into its shell scripts.
#
#   stamp-release-scripts.sh <tag> <src-dir> <dir>
#       Reads scan.sh, install.sh and scan-mdm.sh from <src-dir> (the repo's
#       scripts/, unstamped) and the release tarballs
#       scenery-<version>-<triple>.tar.gz from <dir>, and writes the stamped
#       scripts into <dir>. scan.sh and install.sh get the tag and the SHA-256
#       of every tarball they may download. Then scan-mdm.sh gets the tag and
#       the SHA-256 of the STAMPED scan.sh, so the order is fixed.
#
#   stamp-release-scripts.sh --check <tag> <dir>
#       Verifies the stamped scripts in <dir> against <dir>/SHA256SUMS: each
#       script is listed there with a matching hash, every pinned tarball hash
#       equals its SHA256SUMS line, scan-mdm.sh pins this scan.sh, and every
#       stamp value is the expected one for this tag. Tarballs need not be in
#       <dir>. It does not compare the rest of each script with the repo: in
#       sign-release.sh, `gh attestation verify` proves the scripts came from
#       the release workflow at this tag.
#
# The stamp is the block between the two marker lines below. In the repo copy
# every STAMP_* value is "" and the scripts require a signature check; a
# stamped copy verifies against its pinned hashes without minisign or gh.
# Exit 0 ok, 1 on any problem, 2 usage.
set -eu

SCRIPTS="scan.sh install.sh scan-mdm.sh"
BEGIN_MARK='# >>> scenery release stamp >>>'
END_MARK='# <<< scenery release stamp <<<'

die()   { printf 'stamp-release-scripts: %s\n' "$*" >&2; exit 1; }
usage() { echo "usage: $0 <tag> <src-dir> <dir> | $0 --check <tag> <dir>" >&2; exit 2; }

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
    else shasum -a 256 "$1" | awk '{print $1}'; fi
}

check_tag() {
    case "$1" in v[0-9]*.[0-9]*.[0-9]*) ;; *) die "tag must look like v2.8.0 (got: $1)" ;; esac
    case "$1" in *[!A-Za-z0-9._-]*) die "tag contains unexpected characters: $1" ;; esac
}

# block_vars <file>: prints "NAME VALUE" per STAMP_ line of the one stamp
# block. Fails if the block is missing, repeated, unterminated or malformed.
block_vars() {
    awk -v b="${BEGIN_MARK}" -v e="${END_MARK}" '
        $0 == b { nb++; if (inb) bad = 1; inb = 1; next }
        $0 == e { ne++; if (!inb) bad = 1; inb = 0; next }
        inb {
            if ($0 ~ /^#/ || $0 ~ /^[ \t]*$/) next
            if ($0 !~ /^STAMP_[A-Za-z0-9_]+="[A-Za-z0-9._-]*"$/) { bad = 1; next }
            eq = index($0, "=")
            v = substr($0, eq + 2); v = substr(v, 1, length(v) - 1)
            print substr($0, 1, eq - 1), v; nv++
        }
        END { if (nb != 1 || ne != 1 || inb || bad || nv == 0) exit 1 }
    ' "$1"
}

# fill <in> <out> <mapfile|-blank>: rewrites each STAMP_ line of the block with
# its value from mapfile ("NAME VALUE" lines; every name must be there), or
# with "" for -blank.
fill() {
    awk -v b="${BEGIN_MARK}" -v e="${END_MARK}" -v mapf="$3" '
        BEGIN {
            if (mapf != "-blank") while ((getline l < mapf) > 0) {
                n = split(l, a, " "); if (n == 2) m[a[1]] = a[2]
            }
        }
        $0 == b { inb = 1; print; next }
        $0 == e { inb = 0; print; next }
        inb && /^STAMP_/ {
            name = substr($0, 1, index($0, "=") - 1)
            if (mapf == "-blank") { print name "=\"\""; next }
            if (!(name in m) || m[name] == "") miss = 1
            print name "=\"" m[name] "\""; next
        }
        { print }
        END { if (miss) exit 1 }
    ' "$1" > "$2"
}

# tarball_sha <key> <sums>: the hash of scenery-<VER>-<triple>.tar.gz in a
# SHA256SUMS-format file, where <key> is the triple with - replaced by _.
tarball_sha() {
    awk -v key="$1" -v ver="${VER}" '
        {
            f = $2; sub(/^\*/, "", f)
            p = "scenery-" ver "-"
            if (substr(f, 1, length(p)) != p || f !~ /\.tar\.gz$/) next
            t = substr(f, length(p) + 1); t = substr(t, 1, length(t) - 7); gsub(/-/, "_", t)
            if (t == key) { print tolower($1); exit }
        }
    ' "$2"
}

# build_map <file> <dir> <sums> <mapout>: the expected value of every STAMP_
# name in <file>'s block.
build_map() {
    block_vars "$1" > "${TMP}/vars" || die "$1: missing or malformed release stamp block"
    : > "$4"
    while read -r _n _v; do
        case "${_n}" in
            STAMP_TAG)          _val="${TAG}" ;;
            STAMP_SCAN_SHA256)  [ -f "$2/scan.sh" ] || die "scan.sh must be stamped before $(basename "$1")"
                                _val="$(sha256_of "$2/scan.sh")" ;;
            STAMP_SHA256_*)     _val="$(tarball_sha "${_n#STAMP_SHA256_}" "$3")"
                                [ -n "${_val}" ] || die "$(basename "$1"): no tarball for ${_n#STAMP_SHA256_} (scenery-${VER}-<triple>.tar.gz)" ;;
            *)                  die "$(basename "$1"): unknown stamp variable ${_n}" ;;
        esac
        printf '%s %s\n' "${_n}" "${_val}" >> "$4"
    done < "${TMP}/vars"
}

stamp() {
    SRC="$1"; DIR="$2"
    [ -d "${SRC}" ] && [ -d "${DIR}" ] || usage
    [ "$(cd "${SRC}" && pwd -P)" != "$(cd "${DIR}" && pwd -P)" ] || die "<src-dir> and <dir> must differ"
    : > "${TMP}/tarsums"
    for _f in "${DIR}/scenery-${VER}-"*.tar.gz; do
        [ -f "${_f}" ] || continue
        printf '%s  %s\n' "$(sha256_of "${_f}")" "${_f##*/}" >> "${TMP}/tarsums"
    done
    [ -s "${TMP}/tarsums" ] || die "no scenery-${VER}-*.tar.gz in ${DIR}"
    for _s in ${SCRIPTS}; do
        [ -f "${SRC}/${_s}" ] || die "${SRC}/${_s} not found"
        block_vars "${SRC}/${_s}" > "${TMP}/vars" || die "${_s}: missing or malformed release stamp block"
        while read -r _n _v; do
            [ -z "${_v}" ] || die "${SRC}/${_s} is already stamped (${_n}); stamp the unstamped repo copy"
        done < "${TMP}/vars"
        build_map "${SRC}/${_s}" "${DIR}" "${TMP}/tarsums" "${TMP}/map"
        fill "${SRC}/${_s}" "${TMP}/out" "${TMP}/map" || die "${_s}: could not fill the stamp"
        cp "${TMP}/out" "${DIR}/${_s}"
        echo "stamped ${_s} for ${TAG}"
    done
}

check() {
    DIR="$1"
    [ -d "${DIR}" ] || usage
    SUMS="${DIR}/SHA256SUMS"
    [ -f "${SUMS}" ] || die "${SUMS} not found"
    for _s in ${SCRIPTS}; do
        [ -f "${DIR}/${_s}" ] || die "${_s} not found in ${DIR}"
        _want="$(awk -v f="${_s}" '$2==f || $2=="*" f {print tolower($1); exit}' "${SUMS}")"
        [ -n "${_want}" ] || die "${_s} is not listed in SHA256SUMS"
        [ "$(sha256_of "${DIR}/${_s}")" = "${_want}" ] || die "${_s} does not match SHA256SUMS"
        fill "${DIR}/${_s}" "${TMP}/blank" -blank || die "${_s}: could not read the stamp"
        build_map "${DIR}/${_s}" "${DIR}" "${SUMS}" "${TMP}/map"
        fill "${TMP}/blank" "${TMP}/expect" "${TMP}/map" || die "${_s}: could not fill the stamp"
        cmp -s "${TMP}/expect" "${DIR}/${_s}" \
            || die "${_s} is not stamped for ${TAG} (unstamped, or a wrong tag or hash)"
        echo "ok   ${_s} stamped for ${TAG}"
    done
}

MODE=stamp
if [ "${1:-}" = "--check" ]; then MODE=check; shift; [ $# = 2 ] || usage
else [ $# = 3 ] || usage; fi
TAG="$1"; shift
check_tag "${TAG}"
VER="${TAG#v}"
TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT
if [ "${MODE}" = check ]; then check "$@"; else stamp "$@"; fi
