#!/bin/sh
# VectorHawk runner installer
# Run with:  curl -fsSL https://install.vectorhawk.ai | sh
# Or read first: curl -fsSL https://install.vectorhawk.ai | less

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

[ "${VECTORHAWK_NO_MODIFY_PATH:-0}" = "1" ] && NO_MODIFY_PATH=1
[ "${VECTORHAWK_VERBOSE:-0}"        = "1" ] && VERBOSE=1

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
        log_error "VectorHawk does not yet support ${OS}/${ARCH}."
        log_error "See https://github.com/vectorhawk/vectorhawkd for supported platforms."
        exit 1
        ;;
esac

# ---------------------------------------------------------------------------
# Downloader
# ---------------------------------------------------------------------------

if command -v curl >/dev/null 2>&1; then
    download_url() { curl -fsSL --retry 3 --retry-delay 2 -o "$2" "$1"; }
    fetch_stdout() { curl -fsSL --retry 3 --retry-delay 2 "$1"; }
elif command -v wget >/dev/null 2>&1; then
    download_url() { wget -q --tries=3 --waitretry=2 -O "$2" "$1"; }
    fetch_stdout() { wget -q --tries=3 --waitretry=2 -O - "$1"; }
else
    log_error "Neither curl nor wget found. Install one and retry."
    exit 1
fi

# ---------------------------------------------------------------------------
# Resolve version
# ---------------------------------------------------------------------------

GITHUB_API="https://api.github.com/repos/vectorhawk/vectorhawkd/releases/latest"

if [ -n "${VECTORHAWK_VERSION:-}" ]; then
    TAG="${VECTORHAWK_VERSION}"
    VERSION="${TAG#v}"
else
    log "Fetching latest release..."
    _JSON="$(fetch_stdout "${GITHUB_API}")"

    if command -v python3 >/dev/null 2>&1; then
        TAG="$(printf '%s' "${_JSON}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["tag_name"])')"
    else
        TAG="$(printf '%s' "${_JSON}" | grep -o '"tag_name": *"[^"]*"' | sed 's/"tag_name": *"//;s/"//')"
    fi

    [ -z "${TAG}" ] && { log_error "Could not determine latest version. Set VECTORHAWK_VERSION=vX.Y.Z to override."; exit 1; }
    VERSION="${TAG#v}"
fi

log_verbose "Version: ${TAG} (${VERSION})"

TARBALL="vectorhawk-${VERSION}-${TRIPLE}.tar.gz"
TARBALL_URL="https://github.com/vectorhawk/vectorhawkd/releases/download/${TAG}/${TARBALL}"

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

VH_BIN="${INSTALL_DIR}/vectorhawk"

if [ -x "${VH_BIN}" ]; then
    _CURRENT="$("${VH_BIN}" --version 2>/dev/null | grep -o '[0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*' | head -1 || true)"
    if [ "${_CURRENT}" = "${VERSION}" ]; then
        log "vectorhawk ${VERSION} is already installed."
        if [ "${NO_SETUP}" = "0" ]; then
            log ""
            "${VH_BIN}" daemon install 2>/dev/null || true
            "${VH_BIN}" mcp setup     2>/dev/null || true
        fi
        exit 0
    fi
fi

# ---------------------------------------------------------------------------
# Download and install
# ---------------------------------------------------------------------------

TMPDIR="$(mktemp -d)"
trap 'rm -rf "${TMPDIR}"' EXIT INT TERM

log "Downloading vectorhawk ${VERSION} for ${TRIPLE}..."
download_url "${TARBALL_URL}" "${TMPDIR}/${TARBALL}"

log_verbose "Extracting..."
tar -xzf "${TMPDIR}/${TARBALL}" -C "${TMPDIR}"

mkdir -p "${INSTALL_DIR}"

for _bin in vectorhawk vectorhawkd vectorhawkd-shim; do
    # Support both flat tarballs (./binary) and subdirectoried ones
    # (vectorhawk-VERSION-TRIPLE/binary) by searching after extraction.
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
            printf '\n# Added by VectorHawk installer\n%s\n' "${_LINE}" >> "${_RC}"
            log "Added ${INSTALL_DIR} to PATH in ${_RC}."
        fi
        export PATH="${INSTALL_DIR}:${PATH}"
    else
        log ""
        log "Add this to your shell config to put vectorhawk on PATH:"
        log "    export PATH=\"${INSTALL_DIR}:\$PATH\""
    fi
fi

# ---------------------------------------------------------------------------
# Post-install: start daemon + configure AI clients
# ---------------------------------------------------------------------------

log ""
log "vectorhawk ${VERSION} installed to ${INSTALL_DIR}."
log ""

if [ "${NO_SETUP}" = "0" ]; then
    log "Starting daemon..."
    # On upgrade, restart the daemon so it picks up the new binary.
    if command -v systemctl >/dev/null 2>&1; then
        XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
        export XDG_RUNTIME_DIR
        systemctl --user restart vectorhawk-agent.service 2>/dev/null || true
    fi
    "${VH_BIN}" daemon install || {
        log "  (daemon start deferred — run 'vectorhawk daemon install' after logging in)"
    }

    log "Configuring AI clients..."
    "${VH_BIN}" mcp setup || {
        log "  (client config deferred — run 'vectorhawk mcp setup' in your login shell)"
    }

    log ""
    log "Done. Restart Claude Code (or your AI client), then call the"
    log "vectorhawk_login tool to authenticate."
else
    log "Skipped daemon install and mcp setup (--no-setup)."
    log "Run when ready:"
    log "  vectorhawk daemon install"
    log "  vectorhawk mcp setup"
fi
