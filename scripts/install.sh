#!/bin/sh
# Scenery runner installer
# Run with:  curl -fsSL https://install.scenery.dev | sh
# Or read first: curl -fsSL https://install.scenery.dev | less

set -eu

# ---------------------------------------------------------------------------
# Flags
# ---------------------------------------------------------------------------

SYSTEM_INSTALL=0
NO_MODIFY_PATH=0
NO_SETUP=0
VERBOSE=0

for _arg in "$@"; do
    case "${_arg}" in
        --system)          SYSTEM_INSTALL=1 ;;
        --no-modify-path)  NO_MODIFY_PATH=1 ;;
        --no-setup)        NO_SETUP=1 ;;
        --verbose)         VERBOSE=1 ;;
        *)
            printf 'Unknown flag: %s\n' "${_arg}" >&2
            printf 'Usage: install.sh [--system] [--no-modify-path] [--no-setup] [--verbose]\n' >&2
            exit 1
            ;;
    esac
done

[ "${SCENERY_NO_MODIFY_PATH:-0}" = "1" ] && NO_MODIFY_PATH=1
[ "${SCENERY_VERBOSE:-0}"        = "1" ] && VERBOSE=1

# ---------------------------------------------------------------------------
# Logging
# ---------------------------------------------------------------------------

log()         { printf '%s\n' "$*"; }
log_verbose() { [ "${VERBOSE}" = "1" ] && printf '[verbose] %s\n' "$*" || true; }
log_error()   { printf 'error: %s\n' "$*" >&2; }

# ---------------------------------------------------------------------------
# Platform detection
# ---------------------------------------------------------------------------

OS="$(uname -s)"
ARCH="$(uname -m)"
log_verbose "Detected OS=${OS} ARCH=${ARCH}"

case "${OS}:${ARCH}" in
    Darwin:arm64)   TRIPLE="aarch64-apple-darwin" ;;
    Darwin:x86_64)  TRIPLE="x86_64-apple-darwin" ;;
    Linux:x86_64)   TRIPLE="x86_64-unknown-linux-gnu" ;;
    *)
        log_error "Scenery does not yet support ${OS}/${ARCH}."
        log_error "See https://github.com/scenery-dev/scenery for supported platforms."
        exit 1
        ;;
esac

# ---------------------------------------------------------------------------
# Downloader
# ---------------------------------------------------------------------------

if command -v curl >/dev/null 2>&1; then
    # --proto '=https' --tlsv1.2: refuse any non-HTTPS URL (including on
    # redirects) and floor the TLS version, so a hijacked redirect can't
    # downgrade the transport.
    download_url() { curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 2 -o "$2" "$1"; }
    fetch_stdout() { curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 2 "$1"; }
elif command -v wget >/dev/null 2>&1; then
    download_url() { wget --https-only -q --tries=3 --waitretry=2 -O "$2" "$1"; }
    fetch_stdout() { wget --https-only -q --tries=3 --waitretry=2 -O - "$1"; }
else
    log_error "Neither curl nor wget found. Install one and retry."
    exit 1
fi

# ---------------------------------------------------------------------------
# Checksum
# ---------------------------------------------------------------------------

# Print the lowercase hex SHA-256 of a file, or return non-zero if no SHA-256
# tool is available. Tries sha256sum, then shasum, then openssl.
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

# ---------------------------------------------------------------------------
# Resolve version
# ---------------------------------------------------------------------------

GITHUB_API="https://api.github.com/repos/scenery-dev/scenery/releases/latest"

if [ -n "${SCENERY_VERSION:-}" ]; then
    TAG="${SCENERY_VERSION}"
    VERSION="${TAG#v}"
else
    log "Fetching latest release..."
    _JSON="$(fetch_stdout "${GITHUB_API}")"

    if command -v python3 >/dev/null 2>&1; then
        TAG="$(printf '%s' "${_JSON}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["tag_name"])')"
    else
        TAG="$(printf '%s' "${_JSON}" | grep -o '"tag_name": *"[^"]*"' | sed 's/"tag_name": *"//;s/"//')"
    fi

    [ -z "${TAG}" ] && { log_error "Could not determine latest version. Set SCENERY_VERSION=vX.Y.Z to override."; exit 1; }
    VERSION="${TAG#v}"
fi

log_verbose "Version: ${TAG} (${VERSION})"

TARBALL="scenery-${VERSION}-${TRIPLE}.tar.gz"
TARBALL_URL="https://github.com/scenery-dev/scenery/releases/download/${TAG}/${TARBALL}"

# ---------------------------------------------------------------------------
# Install location
# ---------------------------------------------------------------------------

if [ "${SYSTEM_INSTALL}" = "1" ] && [ -d "/usr/local/bin" ] && [ -w "/usr/local/bin" ]; then
    INSTALL_DIR="/usr/local/bin"
else
    INSTALL_DIR="${HOME}/.local/bin"
fi

log_verbose "Install directory: ${INSTALL_DIR}"

# ---------------------------------------------------------------------------
# Skip if already up to date
# ---------------------------------------------------------------------------

SCENERY_BIN="${INSTALL_DIR}/scenery"

_PREV_VERSION=""
if [ -x "${SCENERY_BIN}" ]; then
    _CURRENT="$("${SCENERY_BIN}" --version 2>/dev/null | grep -o '[0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*' | head -1 || true)"
    if [ "${_CURRENT}" = "${VERSION}" ]; then
        log "scenery ${VERSION} is already installed."
        if [ "${NO_SETUP}" = "0" ]; then
            log ""
            "${SCENERY_BIN}" daemon install 2>/dev/null || true
            "${SCENERY_BIN}" mcp setup     2>/dev/null || true
        fi
        exit 0
    fi
    _PREV_VERSION="${_CURRENT}"
fi

# ---------------------------------------------------------------------------
# Download and install
# ---------------------------------------------------------------------------

TMPDIR="$(mktemp -d)"
trap 'rm -rf "${TMPDIR}"' EXIT INT TERM

log "Downloading scenery ${VERSION} for ${TRIPLE}..."
download_url "${TARBALL_URL}" "${TMPDIR}/${TARBALL}"

# ---------------------------------------------------------------------------
# Verify integrity against the published SHA-256 BEFORE extracting or running
# anything from the tarball. A mismatch (corruption or tampering) aborts.
# ---------------------------------------------------------------------------

log_verbose "Verifying checksum..."
download_url "${TARBALL_URL}.sha256" "${TMPDIR}/${TARBALL}.sha256"

_EXPECTED="$(awk '{print $1}' "${TMPDIR}/${TARBALL}.sha256" | tr '[:upper:]' '[:lower:]')"
if [ -z "${_EXPECTED}" ]; then
    log_error "Empty or missing checksum for ${TARBALL} — refusing to install."
    exit 1
fi

_ACTUAL="$(sha256_of "${TMPDIR}/${TARBALL}" | tr '[:upper:]' '[:lower:]')" || {
    log_error "No SHA-256 tool (sha256sum, shasum, or openssl) found — cannot verify the download."
    log_error "Install one and retry, or use Homebrew: brew tap scenery-dev/tap && brew trust scenery-dev/tap && brew install scenery-dev/tap/scenery"
    exit 1
}

if [ "${_ACTUAL}" != "${_EXPECTED}" ]; then
    log_error "Checksum mismatch for ${TARBALL} — refusing to install."
    log_error "  expected: ${_EXPECTED}"
    log_error "  actual:   ${_ACTUAL}"
    log_error "This means a corrupted download or a tampered artifact. Aborting."
    exit 1
fi
log_verbose "Checksum verified: ${_ACTUAL}"

log_verbose "Extracting..."
tar -xzf "${TMPDIR}/${TARBALL}" -C "${TMPDIR}"

mkdir -p "${INSTALL_DIR}"

for _bin in scenery; do
    # Support both flat tarballs (./binary) and subdirectoried ones
    # (scenery-VERSION-TRIPLE/binary) by searching after extraction.
    _src="$(find "${TMPDIR}" -name "${_bin}" -type f | head -1)"
    if [ -z "${_src}" ]; then
        log_error "Expected binary not in tarball: ${_bin}"
        exit 1
    fi
    cp "${_src}" "${INSTALL_DIR}/${_bin}.tmp"
    mv "${INSTALL_DIR}/${_bin}.tmp" "${INSTALL_DIR}/${_bin}"
    chmod 755 "${INSTALL_DIR}/${_bin}"
    log_verbose "Installed ${_bin}"
done

# `scn` is a short alias for `scenery`: a symlink next to the binary. The
# binary resolves symlinks (current_exe), so `scn` behaves identically.
ln -sf scenery "${INSTALL_DIR}/scn.tmp"
mv "${INSTALL_DIR}/scn.tmp" "${INSTALL_DIR}/scn"
log_verbose "Installed scn -> scenery"

# ---------------------------------------------------------------------------
# PATH
# ---------------------------------------------------------------------------

_path_check=":${PATH}:"
case "${_path_check}" in *":${INSTALL_DIR}:"*) _on_path=1 ;; *) _on_path=0 ;; esac

if [ "${_on_path}" = "0" ] && [ "${SYSTEM_INSTALL}" = "0" ] && [ "${NO_MODIFY_PATH}" = "0" ]; then
    _SHELL_NAME="$(basename "${SHELL:-sh}")"
    case "${_SHELL_NAME}" in
        zsh)  _RC="${HOME}/.zshrc";                       _LINE="export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
        bash) _RC="${HOME}/.bashrc";                      _LINE="export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
        fish) _RC="${HOME}/.config/fish/config.fish";     _LINE="fish_add_path \"${INSTALL_DIR}\"" ;;
        *)    _RC=""; _LINE="" ;;
    esac

    if [ -n "${_RC}" ]; then
        if ! grep -qF "${INSTALL_DIR}" "${_RC}" 2>/dev/null; then
            printf '\n# Added by Scenery installer\n%s\n' "${_LINE}" >> "${_RC}"
            log "Added ${INSTALL_DIR} to PATH in ${_RC}."
        fi
        export PATH="${INSTALL_DIR}:${PATH}"
    else
        log ""
        log "Add this to your shell config to put scenery on PATH:"
        log "    export PATH=\"${INSTALL_DIR}:\$PATH\""
    fi
fi

# ---------------------------------------------------------------------------
# Post-install: start daemon + configure AI clients
# ---------------------------------------------------------------------------

log ""
log "scenery ${VERSION} installed to ${INSTALL_DIR}."
log ""

# ---------------------------------------------------------------------------
# Pairing helpers
# ---------------------------------------------------------------------------

# `scenery auth pair` accepts a bare (code-less) invocation starting in
# this version — older installed binaries require the code as a required
# positional argument and fail with a missing-argument error if it's
# omitted.
_PAIR_MIN_VERSION="1.0.89"

# version_at_least MIN HAVE
# Numeric major.minor.patch comparison, POSIX sh only (no `sort -V`, which
# isn't portable to every install target, and no string comparison, under
# which "1.0.9" would wrongly compare as newer than "1.0.89"). Returns
# success (0) if HAVE >= MIN, 1 if HAVE < MIN, and 2 if HAVE doesn't parse
# as three dot-separated numeric components — callers should treat "doesn't
# parse" as unsupported and fall back to the manual instructions rather
# than risk a wrong comparison or an arithmetic error on garbage input.
version_at_least() {
    _val_min="$1"
    _val_have="$2"

    case "${_val_have}" in
        *[!0-9.]*) return 2 ;;
    esac

    _h_maj="${_val_have%%.*}"
    _h_rest="${_val_have#*.}"
    case "${_h_rest}" in
        "${_val_have}") return 2 ;; # no dot at all
    esac
    _h_min="${_h_rest%%.*}"
    _h_pat="${_h_rest#*.}"
    case "${_h_pat}" in
        "${_h_rest}") return 2 ;; # only one dot total (two components)
        *.*)          return 2 ;; # more than two dots (four+ components)
    esac
    case "${_h_maj}" in '') return 2 ;; esac
    case "${_h_min}" in '') return 2 ;; esac
    case "${_h_pat}" in '') return 2 ;; esac

    _m_maj="${_val_min%%.*}"
    _m_rest="${_val_min#*.}"
    _m_min="${_m_rest%%.*}"
    _m_pat="${_m_rest#*.}"

    if [ "${_h_maj}" -gt "${_m_maj}" ] 2>/dev/null; then return 0; fi
    if [ "${_h_maj}" -lt "${_m_maj}" ] 2>/dev/null; then return 1; fi
    if [ "${_h_min}" -gt "${_m_min}" ] 2>/dev/null; then return 0; fi
    if [ "${_h_min}" -lt "${_m_min}" ] 2>/dev/null; then return 1; fi
    if [ "${_h_pat}" -ge "${_m_pat}" ] 2>/dev/null; then return 0; fi
    return 1
}

# A terminal is available if /dev/tty can actually be opened for reading —
# NOT `[ -t 0 ]`. The documented install path is
# `curl -fsSL https://install.scenery.dev | sh`, where stdin IS the
# script being piped in, so `[ -t 0 ]` reads false even though a real
# terminal is sitting right there controlling the process. Opening
# /dev/tty directly is the only reliable check.
terminal_available() {
    : < /dev/tty 2>/dev/null
}

if [ "${NO_SETUP}" = "0" ]; then
    log "Starting daemon..."
    # On upgrade (binary version changed), restart the daemon so it picks up
    # the new binary. Skip the restart on a fresh install — daemon install
    # handles that path. Never restart if _PREV_VERSION is empty (fresh install).
    if [ -n "${_PREV_VERSION}" ] && [ "${_PREV_VERSION}" != "${VERSION}" ] && command -v systemctl >/dev/null 2>&1; then
        XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
        export XDG_RUNTIME_DIR
        systemctl --user restart scenery.service 2>/dev/null || true
    fi
    "${SCENERY_BIN}" daemon install || {
        log "  (daemon start deferred — run 'scenery daemon install' after logging in)"
    }

    log "Configuring AI clients..."
    "${SCENERY_BIN}" mcp setup || {
        log "  (client config deferred — run 'scenery mcp setup' in your login shell)"
    }

    log ""
    _AUTHENTICATED=0
    # Everything below needs a binary new enough to (a) exit non-zero from
    # `auth status` when it isn't logged in, and (b) accept `auth pair` with
    # no positional code. Older binaries exit 0 from `auth status`
    # regardless of auth state, so trusting the exit code there would
    # wrongly report "already authenticated" and skip the instructions
    # entirely. One version guard covers both.
    if ! version_at_least "${_PAIR_MIN_VERSION}" "${VERSION#v}"; then
        log "To finish setup, pair this device: find the code on the portal's device"
        log "setup screen (open the catalog page), then run:"
        log "  scenery auth pair <code>"
    else
        log "Checking authentication..."
        if "${SCENERY_BIN}" auth status >/dev/null 2>&1; then
            _AUTHENTICATED=1
            log "Already authenticated — skipping pairing."
        else
            if [ -n "${SCN_PAIR_CODE:-}" ]; then
                # Unattended path (MDM / Intune / Jamf): the CLI reads
                # SCN_PAIR_CODE itself. No prompt, no terminal needed — this
                # is the only pairing path that works with no tty at all.
                log "Pairing this device using SCN_PAIR_CODE..."
                if "${SCENERY_BIN}" auth pair; then
                    _AUTHENTICATED=1
                else
                    log "  (pairing failed — run 'scenery auth pair <code>' manually when ready)"
                fi
            elif terminal_available; then
                log "Pairing this device — find the code on the portal's device setup screen"
                log "(open the catalog page; if this device isn't registered yet, you'll see it there)."
                if "${SCENERY_BIN}" auth pair < /dev/tty; then
                    _AUTHENTICATED=1
                else
                    log "  (pairing failed or declined — run 'scenery auth pair <code>' manually when ready)"
                fi
            else
                # No terminal (CI/MDM without SCN_PAIR_CODE) — best-effort
                # only, never blocks the install.
                log "To finish setup, pair this device: find the code on the portal's device"
                log "setup screen (open the catalog page), then run:"
                log "  scenery auth pair <code>"
            fi
        fi
    fi

    log ""
    if [ "${_AUTHENTICATED}" = "1" ]; then
        log "Done. Restart Claude Code (or your AI client) to pick up the new setup."
    else
        log "Done. Restart Claude Code (or your AI client), then call the"
        log "scenery_login tool (or run 'scenery auth pair <code>') to authenticate."
    fi
else
    log "Skipped daemon install and mcp setup (--no-setup)."
    log "Run when ready:"
    log "  scenery daemon install"
    log "  scenery mcp setup"
    log "  scenery auth pair <code>"
fi
