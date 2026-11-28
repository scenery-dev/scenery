#!/bin/sh
# Scenery runnerless scan
# Run with:  curl -fsSL https://install.scenery.dev/scan.sh | sh
#   or:      curl -fsSL https://install.scenery.dev/scan.sh | sh -s -- --dry-run
#
# Downloads the signed release for this platform into a temp dir, verifies it
# exactly as install.sh does, runs `scenery scan --once`, and deletes the temp
# dir. It installs nothing: it never writes outside its temp dir and never
# touches PATH, shell rc files or ~/.local/bin.
#
#   SCENERY_SCAN_TOKEN     org scan token (non-interactive). Without it (and
#                          without --token-file) the scan signs you in with --login.
#   SCENERY_SCAN_VERSION   pin a release tag (default: the latest release, or
#                          the stamped tag for a release copy).
#
# Verification. The copy published with a release (and served at
# install.scenery.dev) is STAMPED: the block below holds its tag and the
# SHA-256 of every tarball it may download, so it verifies the download with
# shasum/sha256sum alone; when minisign or an authenticated gh is present it
# also checks the release signature. The unstamped repo copy, or a stamped
# copy asked for a different tag, requires minisign or gh and refuses
# otherwise. Any mismatch refuses to run.

set -eu

# >>> scenery release stamp >>>
# Filled in by scripts/stamp-release-scripts.sh at release time; empty here.
STAMP_TAG=""
STAMP_SHA256_aarch64_apple_darwin=""
STAMP_SHA256_x86_64_apple_darwin=""
STAMP_SHA256_x86_64_unknown_linux_gnu=""
# <<< scenery release stamp <<<

log()       { printf '%s\n' "$*" >&2; }
log_error() { printf 'error: %s\n' "$*" >&2; }

main() {
    OS="$(uname -s)"
    ARCH="$(uname -m)"
    case "${OS}:${ARCH}" in
        Darwin:arm64)   TRIPLE="aarch64-apple-darwin" ;;
        Darwin:x86_64)  TRIPLE="x86_64-apple-darwin" ;;
        Linux:x86_64)   TRIPLE="x86_64-unknown-linux-gnu" ;;
        *)
            log_error "Scenery does not yet support ${OS}/${ARCH}."
            exit 1
            ;;
    esac

    # SCENERY_SCAN_BASE_URL is test-only: it replaces the GitHub release origin
    # and allows plain http so a local server can stand in.
    BASE_URL="${SCENERY_SCAN_BASE_URL:-https://github.com/scenery-dev/scenery/releases/download}"
    if [ -n "${SCENERY_SCAN_BASE_URL:-}" ]; then
        _PROTO='=http,https'
    else
        _PROTO='=https'
    fi

    if command -v curl >/dev/null 2>&1; then
        download_url() { curl --proto "${_PROTO}" --tlsv1.2 -fsSL --retry 3 --retry-delay 2 -o "$2" "$1"; }
        fetch_stdout() { curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 2 "$1"; }
    elif command -v wget >/dev/null 2>&1; then
        download_url() { wget --https-only -q --tries=3 --waitretry=2 -O "$2" "$1"; }
        fetch_stdout() { wget --https-only -q --tries=3 --waitretry=2 -O - "$1"; }
    else
        log_error "Neither curl nor wget found. Install one and retry."
        exit 1
    fi

    sha256_of() {
        if command -v sha256sum >/dev/null 2>&1; then
            sha256sum "$1" | awk '{print $1}'
        elif command -v shasum >/dev/null 2>&1; then
            shasum -a 256 "$1" | awk '{print $1}'
        elif command -v openssl >/dev/null 2>&1; then
            openssl dgst -sha256 "$1" | awk '{print $NF}'
        else
            return 1
        fi
    }

    if [ -n "${SCENERY_SCAN_VERSION:-}" ]; then
        TAG="${SCENERY_SCAN_VERSION}"
        case "${TAG}" in
            v[0-9]*.[0-9]*.[0-9]*) ;;
            *) log_error "SCENERY_SCAN_VERSION must look like v2.8.0 (got: ${TAG})."; exit 1 ;;
        esac
        case "${TAG}" in
            *[!A-Za-z0-9._-]*) log_error "SCENERY_SCAN_VERSION contains unexpected characters."; exit 1 ;;
        esac
    elif [ -n "${STAMP_TAG}" ]; then
        TAG="${STAMP_TAG}"
    else
        _JSON="$(fetch_stdout "https://api.github.com/repos/scenery-dev/scenery/releases/latest")" || {
            log_error "Could not look up the latest release from the GitHub API."
            log_error "Set SCENERY_SCAN_VERSION=vX.Y.Z to skip the lookup."
            exit 1
        }
        if command -v python3 >/dev/null 2>&1; then
            TAG="$(printf '%s' "${_JSON}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["tag_name"])')"
        else
            TAG="$(printf '%s' "${_JSON}" | grep -o '"tag_name": *"[^"]*"' | sed 's/"tag_name": *"//;s/"//')"
        fi
        [ -z "${TAG}" ] && { log_error "Could not determine latest version. Set SCENERY_SCAN_VERSION=vX.Y.Z to override."; exit 1; }
    fi
    VERSION="${TAG#v}"

    TARBALL="scenery-${VERSION}-${TRIPLE}.tar.gz"
    TARBALL_URL="${BASE_URL}/${TAG}/${TARBALL}"
    SUMS_URL="${BASE_URL}/${TAG}/SHA256SUMS"

    WORK="$(mktemp -d)"
    trap 'rm -rf "${WORK}"' EXIT
    trap 'exit 130' INT TERM HUP

    # Release signing key (minisign key id 73F4BD707C872144); same as install.sh.
    MINISIGN_PUBKEY="RWREIYd8cL30cy+6WHSsE+0oVDsfoPu/1BmCGX4rb0Fgs4+ZC3jjW/gp"
    GH_REPO="scenery-dev/scenery"

    # The pinned hash, when this is a stamped copy running its own tag.
    _PIN=""
    if [ -n "${STAMP_TAG}" ] && [ "${TAG}" = "${STAMP_TAG}" ]; then
        case "${TRIPLE}" in
            aarch64-apple-darwin)     _PIN="${STAMP_SHA256_aarch64_apple_darwin}" ;;
            x86_64-apple-darwin)      _PIN="${STAMP_SHA256_x86_64_apple_darwin}" ;;
            x86_64-unknown-linux-gnu) _PIN="${STAMP_SHA256_x86_64_unknown_linux_gnu}" ;;
        esac
        [ -n "${_PIN}" ] || { log_error "No pinned checksum for ${TRIPLE} in this scan.sh — refusing to run."; exit 1; }
    fi

    # Signature verifier: minisign, else gh. With a pin, gh is used only when
    # it is signed in (an unauthenticated gh cannot verify attestations).
    _VERIFIER=""
    if command -v minisign >/dev/null 2>&1; then
        _VERIFIER=minisign
    elif command -v gh >/dev/null 2>&1; then
        if [ -z "${_PIN}" ] || XDG_CACHE_HOME="${WORK}/cache" gh auth status >/dev/null 2>&1; then
            _VERIFIER=gh
        fi
    fi
    if [ -z "${_VERIFIER}" ] && [ -z "${_PIN}" ]; then
        log_error "Cannot verify the release signature: neither minisign nor gh is available — refusing to run."
        log_error "Install one (brew install minisign, or https://cli.github.com), or use the scan.sh"
        log_error "published with the release: curl -fsSL https://install.scenery.dev/scan.sh | sh"
        exit 1
    fi

    log "Downloading scenery ${VERSION} for ${TRIPLE}..."
    download_url "${TARBALL_URL}" "${WORK}/${TARBALL}" || {
        log_error "Could not download ${TARBALL_URL}"
        exit 1
    }
    _ACTUAL="$(sha256_of "${WORK}/${TARBALL}" | tr '[:upper:]' '[:lower:]')" || {
        log_error "No SHA-256 tool (sha256sum, shasum, or openssl) found — cannot verify the download."
        exit 1
    }
    if [ -n "${_PIN}" ] && [ "${_ACTUAL}" != "${_PIN}" ]; then
        log_error "Checksum mismatch for ${TARBALL} against the hash pinned in scan.sh — refusing to run."
        log_error "  expected: ${_PIN}"
        log_error "  actual:   ${_ACTUAL}"
        exit 1
    fi

    if [ -n "${_VERIFIER}" ]; then
        download_url "${SUMS_URL}" "${WORK}/SHA256SUMS" || {
            log_error "Could not download SHA256SUMS for ${TAG} — refusing to run."
            exit 1
        }
        if [ "${_VERIFIER}" = minisign ]; then
            download_url "${SUMS_URL}.minisig" "${WORK}/SHA256SUMS.minisig" || {
                log_error "Could not download SHA256SUMS.minisig — refusing to run."
                exit 1
            }
            if ! minisign -Vm "${WORK}/SHA256SUMS" -x "${WORK}/SHA256SUMS.minisig" -P "${MINISIGN_PUBKEY}" >/dev/null 2>&1; then
                log_error "minisign signature verification FAILED for SHA256SUMS — refusing to run."
                exit 1
            fi
        else
            if ! XDG_CACHE_HOME="${WORK}/cache" gh attestation verify "${WORK}/SHA256SUMS" --repo "${GH_REPO}" >/dev/null 2>&1; then
                log_error "gh attestation verification FAILED for SHA256SUMS — refusing to run."
                log_error "(gh must be authenticated: run 'gh auth login'.)"
                exit 1
            fi
        fi
        _EXPECTED="$(awk -v f="${TARBALL}" '$2==f || $2=="*" f {print $1}' "${WORK}/SHA256SUMS" | head -1 | tr '[:upper:]' '[:lower:]')"
        if [ -z "${_EXPECTED}" ]; then
            log_error "${TARBALL} is not listed in SHA256SUMS — refusing to run."
            exit 1
        fi
        if [ "${_ACTUAL}" != "${_EXPECTED}" ]; then
            log_error "Checksum mismatch for ${TARBALL} — refusing to run."
            log_error "  expected: ${_EXPECTED}"
            log_error "  actual:   ${_ACTUAL}"
            exit 1
        fi
    fi

    mkdir "${WORK}/x"
    tar -xzf "${WORK}/${TARBALL}" -C "${WORK}/x"
    BIN="$(find "${WORK}/x" -name scenery -type f | head -1)"
    if [ -z "${BIN}" ]; then
        log_error "Expected binary not in tarball: scenery"
        exit 1
    fi
    chmod 755 "${BIN}"

    # The scan must not be exec'd: the EXIT trap has to run afterwards.
    _RC=0
    _HAS_TOKEN_FILE=0
    for _a in "$@"; do
        case "${_a}" in --token-file|--token-file=*) _HAS_TOKEN_FILE=1 ;; esac
    done
    if [ -n "${SCENERY_SCAN_TOKEN:-}" ] || [ "${_HAS_TOKEN_FILE}" = 1 ]; then
        "${BIN}" scan --once "$@" </dev/null || _RC=$?
    else
        "${BIN}" scan --once --login "$@" </dev/null || _RC=$?
    fi
    exit "${_RC}"
}

main "$@"
