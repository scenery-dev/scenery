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
#   Arguments            passed through to scan.sh (for example --dry-run).
#
# Exit code: the scan's own exit code (0 ok, 2 refused or not signed in,
# 3 upload failed and a report was saved). 0 when nobody is logged in.
# 1 when scan.sh could not be downloaded.

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

    # /var/tmp, not root's private $TMPDIR: the console user must be able to
    # reach scan.sh and the token file.
    WORK="$(mktemp -d /var/tmp/scenery-scan-mdm.XXXXXX)"
    trap 'rm -rf "${WORK}"' EXIT
    chmod 711 "${WORK}"
    curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 2 \
        -o "${WORK}/scan.sh" "https://install.scenery.dev/scan.sh" || {
        log_error "Could not download scan.sh."
        exit 1
    }
    chmod 644 "${WORK}/scan.sh"

    # Token file: created 0600 by root, then handed to the console user.
    ( umask 077; printf '%s\n' "${SCENERY_SCAN_TOKEN}" > "${WORK}/token" )
    unset SCENERY_SCAN_TOKEN
    chmod 600 "${WORK}/token"
    chown "${USER_NAME}" "${WORK}/token"

    # No `exec`: the EXIT trap must remove the downloaded script and token.
    _RC=0
    if [ "$(uname -s)" = "Darwin" ]; then
        launchctl asuser "${USER_ID}" \
            sudo -H -u "${USER_NAME}" \
            sh "${WORK}/scan.sh" --token-file "${WORK}/token" "$@" || _RC=$?
    else
        sudo -H -u "${USER_NAME}" \
            sh "${WORK}/scan.sh" --token-file "${WORK}/token" "$@" || _RC=$?
    fi
    exit "${_RC}"
}

main "$@"
