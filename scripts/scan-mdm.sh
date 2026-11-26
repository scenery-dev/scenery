#!/bin/sh
# Scenery MDM re-scan wrapper. Runs as root from an MDM (Jamf, Kandji, ...).
#
# Finds the person logged in at the console and runs the runnerless scan
# (scan.sh) as that user, because the scan refuses to run as root and must see
# the user's own home. Nothing is installed and nothing schedules itself: the
# MDM owns the schedule (see docs/scan-mdm.md).
#
#   SCENERY_SCAN_TOKEN   required. Org scan token. It is read from this
#                        script's environment, written to a 0600 file owned by
#                        the console user, and handed to the scan as
#                        --token-file <path>. The token is never in any
#                        command line; only the file path is. The file is
#                        removed when the script exits.
#   SCENERY_SCAN_VERSION optional release tag (v2.8.0). Default: the latest
#                        release. scan.sh is fetched from that tag and the
#                        scan runs the same tag.
#   SCENERY_REGISTRY_URL optional backend URL for non-prod targets. sudo -H
#                        resets the environment, so this and the tag are
#                        forwarded to scan.sh explicitly (neither is secret).
#   Arguments            passed through to scan.sh (for example --dry-run).
#
# scan.sh is downloaded from the GitHub release for the tag and checked
# against that release's SHA256SUMS, whose signature is verified with
# minisign (or `gh attestation verify` when minisign is missing), as scan.sh
# and install.sh do. Any failure refuses to run.
#
# Exit code: the scan's own exit code (0 ok, 2 refused or not signed in,
# 3 upload failed and a report was saved). 0 when nobody is logged in.
# 1 when scan.sh could not be downloaded or verified.

set -eu

log()       { printf '%s\n' "$*" >&2; }
log_error() { printf 'error: %s\n' "$*" >&2; }

console_user() {
    case "$(uname -s)" in
        Darwin)
            stat -f%Su /dev/console 2>/dev/null || true
            ;;
        Linux)
            command -v loginctl >/dev/null 2>&1 || return 0
            loginctl list-sessions --no-legend 2>/dev/null | awk '{print $1}' | while read -r sid; do
                [ -n "${sid}" ] || continue
                props="$(loginctl show-session "${sid}" -p Name -p Active -p Class -p Type 2>/dev/null)" || continue
                case "${props}" in
                    *Active=yes*) ;;
                    *) continue ;;
                esac
                case "${props}" in
                    *Class=user*) ;;
                    *) continue ;;
                esac
                case "${props}" in
                    *Type=x11*|*Type=wayland*|*Type=mir*) ;;
                    *) continue ;;
                esac
                printf '%s\n' "${props}" | sed -n 's/^Name=//p' | head -n 1
                break
            done
            ;;
    esac
}

fetch() { curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 2 "$@"; }

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

# Sets TAG from SCENERY_SCAN_VERSION or the latest release.
resolve_tag() {
    if [ -n "${SCENERY_SCAN_VERSION:-}" ]; then
        TAG="${SCENERY_SCAN_VERSION}"
        case "${TAG}" in
            v[0-9]*.[0-9]*.[0-9]*) ;;
            *) log_error "SCENERY_SCAN_VERSION must look like v2.8.0 (got: ${TAG})."; exit 1 ;;
        esac
        case "${TAG}" in
            *[!A-Za-z0-9._-]*) log_error "SCENERY_SCAN_VERSION contains unexpected characters."; exit 1 ;;
        esac
        return 0
    fi
    _JSON="$(fetch "https://api.github.com/repos/scenery-dev/scenery/releases/latest")" || {
        log_error "Could not look up the latest release. Set SCENERY_SCAN_VERSION=vX.Y.Z."
        exit 1
    }
    TAG="$(printf '%s' "${_JSON}" | grep -o '"tag_name": *"[^"]*"' | head -n 1 | sed 's/"tag_name": *"//;s/"//')"
    case "${TAG}" in
        v[0-9]*.[0-9]*.[0-9]*) ;;
        *) log_error "Could not determine the latest release. Set SCENERY_SCAN_VERSION=vX.Y.Z."; exit 1 ;;
    esac
    case "${TAG}" in
        *[!A-Za-z0-9._-]*) log_error "Unexpected release tag from the GitHub API."; exit 1 ;;
    esac
}

# Downloads scan.sh for TAG into WORK and verifies it. Exits 1 on any failure.
fetch_verified_scan() {
    _BASE="https://github.com/scenery-dev/scenery/releases/download/${TAG}"
    fetch -o "${WORK}/scan.sh" "${_BASE}/scan.sh" || {
        log_error "Could not download scan.sh for ${TAG} — refusing to run."
        exit 1
    }
    fetch -o "${WORK}/SHA256SUMS" "${_BASE}/SHA256SUMS" || {
        log_error "Could not download SHA256SUMS for ${TAG} — refusing to run."
        exit 1
    }

    # Release signing key (minisign key id 73F4BD707C872144); same as scan.sh.
    _PUBKEY="RWREIYd8cL30cy+6WHSsE+0oVDsfoPu/1BmCGX4rb0Fgs4+ZC3jjW/gp"
    if command -v minisign >/dev/null 2>&1; then
        fetch -o "${WORK}/SHA256SUMS.minisig" "${_BASE}/SHA256SUMS.minisig" || {
            log_error "Could not download SHA256SUMS.minisig — refusing to run."
            exit 1
        }
        if ! minisign -Vm "${WORK}/SHA256SUMS" -x "${WORK}/SHA256SUMS.minisig" -P "${_PUBKEY}" >/dev/null 2>&1; then
            log_error "minisign signature verification FAILED for SHA256SUMS — refusing to run."
            exit 1
        fi
    elif command -v gh >/dev/null 2>&1; then
        if ! XDG_CACHE_HOME="${WORK}/cache" gh attestation verify "${WORK}/SHA256SUMS" --repo scenery-dev/scenery >/dev/null 2>&1; then
            log_error "gh attestation verification FAILED for SHA256SUMS — refusing to run."
            exit 1
        fi
    else
        log_error "Cannot verify the release signature: neither minisign nor gh is available — refusing to run."
        exit 1
    fi

    _EXPECTED="$(awk '$2=="scan.sh" || $2=="*scan.sh" {print $1}' "${WORK}/SHA256SUMS" | head -n 1 | tr '[:upper:]' '[:lower:]')"
    [ -n "${_EXPECTED}" ] || { log_error "scan.sh is not listed in SHA256SUMS for ${TAG} — refusing to run."; exit 1; }
    _ACTUAL="$(sha256_of "${WORK}/scan.sh" | tr '[:upper:]' '[:lower:]')" || {
        log_error "No SHA-256 tool (sha256sum, shasum, or openssl) found — refusing to run."
        exit 1
    }
    if [ "${_ACTUAL}" != "${_EXPECTED}" ]; then
        log_error "Checksum mismatch for scan.sh — refusing to run."
        exit 1
    fi
}

# Runs "$@" as the console user.
as_user() {
    if [ "$(uname -s)" = "Darwin" ]; then
        launchctl asuser "${USER_ID}" sudo -H -u "${USER_NAME}" "$@"
    else
        sudo -H -u "${USER_NAME}" "$@"
    fi
}

main() {
    if [ -z "${SCENERY_SCAN_TOKEN:-}" ]; then
        log_error "SCENERY_SCAN_TOKEN is not set. Pass the org scan token in the environment."
        exit 2
    fi
    USER_NAME="$(console_user)"
    case "${USER_NAME}" in
        ""|root|loginwindow|_mbsetupuser)
            log "no user logged in; nothing scanned"
            exit 0
            ;;
    esac
    USER_ID="$(id -u "${USER_NAME}")"
    resolve_tag

    # /var/tmp, not root's private $TMPDIR: the console user must be able to
    # reach scan.sh and the token file.
    WORK="$(mktemp -d /var/tmp/scenery-scan-mdm.XXXXXX)"
    trap 'rm -rf "${WORK}"' EXIT
    chmod 711 "${WORK}"
    fetch_verified_scan
    chmod 644 "${WORK}/scan.sh"

    # Token file: created 0600 by root, then handed to the console user.
    ( umask 077; printf '%s\n' "${SCENERY_SCAN_TOKEN}" > "${WORK}/token" )
    unset SCENERY_SCAN_TOKEN
    chmod 600 "${WORK}/token"
    chown "${USER_NAME}" "${WORK}/token"

    # No `exec`: the EXIT trap must remove the downloaded script and token.
    # sudo -H resets the environment: pass the tag (so the scan runs the
    # binary from the same release) and the registry URL explicitly.
    _RC=0
    if [ -n "${SCENERY_REGISTRY_URL:-}" ]; then
        as_user env SCENERY_SCAN_VERSION="${TAG}" SCENERY_REGISTRY_URL="${SCENERY_REGISTRY_URL}" \
            sh "${WORK}/scan.sh" --token-file "${WORK}/token" "$@" || _RC=$?
    else
        as_user env SCENERY_SCAN_VERSION="${TAG}" \
            sh "${WORK}/scan.sh" --token-file "${WORK}/token" "$@" || _RC=$?
    fi
    exit "${_RC}"
}

main "$@"
