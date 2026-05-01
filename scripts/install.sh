#!/bin/sh
# VectorHawk runner installer. Source: https://github.com/vectorhawk/vectorhawkd/blob/main/scripts/install.sh
# Run with:  curl -fsSL https://install.vectorhawk.ai | sh
# Or read first: curl -fsSL https://install.vectorhawk.ai | less

set -euf

# ---------------------------------------------------------------------------
# Parse flags
# ---------------------------------------------------------------------------

SYSTEM_INSTALL=0
NO_MODIFY_PATH=0
VERBOSE=0

for _arg in "$@"; do
    case "${_arg}" in
        --system)          SYSTEM_INSTALL=1 ;;
        --no-modify-path)  NO_MODIFY_PATH=1 ;;
        --verbose)         VERBOSE=1 ;;
        *)
            printf 'Unknown flag: %s\n' "${_arg}" >&2
            printf 'Usage: install.sh [--system] [--no-modify-path] [--verbose]\n' >&2
            exit 1
            ;;
    esac
done

# Environment variable overrides for flags (checked after arg parsing so args win).
if [ "${VECTORHAWK_NO_MODIFY_PATH:-0}" = "1" ]; then
    NO_MODIFY_PATH=1
fi
if [ "${VECTORHAWK_VERBOSE:-0}" = "1" ]; then
    VERBOSE=1
fi

# ---------------------------------------------------------------------------
# Logging helpers
# ---------------------------------------------------------------------------

log_info() {
    printf '%s\n' "$*"
}

log_verbose() {
    if [ "${VERBOSE}" = "1" ]; then
        printf '[verbose] %s\n' "$*"
    fi
}

log_error() {
    printf 'error: %s\n' "$*" >&2
}

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
        log_error "VectorHawk runner does not yet support ${OS}/${ARCH}."
        log_error "See https://github.com/vectorhawk/vectorhawkd for supported platforms."
        exit 1
        ;;
esac

log_verbose "Target triple: ${TRIPLE}"

# ---------------------------------------------------------------------------
# Downloader detection (curl preferred, wget fallback)
# ---------------------------------------------------------------------------

_DOWNLOADER=""

if command -v curl >/dev/null 2>&1; then
    _DOWNLOADER="curl"
elif command -v wget >/dev/null 2>&1; then
    _DOWNLOADER="wget"
else
    log_error "Neither curl nor wget is available."
    log_error "Install one of them and re-run this script."
    exit 1
fi

log_verbose "Downloader: ${_DOWNLOADER}"

# Unified download function: download_url <url> <dest_file>
download_url() {
    _dl_url="$1"
    _dl_dest="$2"
    log_verbose "Downloading ${_dl_url}"
    if [ "${_DOWNLOADER}" = "curl" ]; then
        curl -fsSL --retry 3 --retry-delay 2 -o "${_dl_dest}" "${_dl_url}"
    else
        wget -q --tries=3 --waitretry=2 -O "${_dl_dest}" "${_dl_url}"
    fi
}

# Fetch URL to stdout (for API JSON).
fetch_stdout() {
    _fs_url="$1"
    log_verbose "Fetching ${_fs_url}"
    if [ "${_DOWNLOADER}" = "curl" ]; then
        curl -fsSL --retry 3 --retry-delay 2 "${_fs_url}"
    else
        wget -q --tries=3 --waitretry=2 -O - "${_fs_url}"
    fi
}

# ---------------------------------------------------------------------------
# Resolve the version to install
# ---------------------------------------------------------------------------

GITHUB_API="https://api.github.com/repos/vectorhawk/vectorhawkd/releases/latest"

# Allow env-var override: VECTORHAWK_VERSION=v0.1.0 (tag form, with leading v).
if [ -n "${VECTORHAWK_VERSION:-}" ]; then
    TAG="${VECTORHAWK_VERSION}"
    # Strip leading 'v' to get the bare version for tarball naming.
    VERSION="${TAG#v}"
    log_verbose "Version override: tag=${TAG} version=${VERSION}"
else
    log_info "Fetching latest release information..."
    _RELEASE_JSON=""

    if command -v python3 >/dev/null 2>&1; then
        _RELEASE_JSON="$(fetch_stdout "${GITHUB_API}")"
        TAG="$(printf '%s' "${_RELEASE_JSON}" | python3 -c \
            'import json,sys; d=json.load(sys.stdin); print(d["tag_name"])')"
    else
        _RELEASE_JSON="$(fetch_stdout "${GITHUB_API}")"
        # POSIX grep/sed extraction. The GitHub API response uses "tag_name": "v..."
        # (with a space after the colon). The sed strips the key and surrounding
        # quotes to leave just the bare tag value.
        TAG="$(printf '%s' "${_RELEASE_JSON}" | \
            grep -o '"tag_name": *"[^"]*"' | \
            sed 's/"tag_name": *"//;s/"//')"
    fi

    if [ -z "${TAG}" ]; then
        log_error "Could not determine latest release tag from GitHub API."
        log_error "Check your network connection or set VECTORHAWK_VERSION=<tag> to install a specific version."
        exit 1
    fi

    VERSION="${TAG#v}"
    log_verbose "Latest release: tag=${TAG} version=${VERSION}"
fi

TARBALL_NAME="vectorhawk-${VERSION}-${TRIPLE}.tar.gz"
SHA256_NAME="${TARBALL_NAME}.sha256"
DOWNLOAD_BASE="https://github.com/vectorhawk/vectorhawkd/releases/download/${TAG}"
TARBALL_URL="${DOWNLOAD_BASE}/${TARBALL_NAME}"
SHA256_URL="${DOWNLOAD_BASE}/${SHA256_NAME}"

log_verbose "Tarball URL: ${TARBALL_URL}"
log_verbose "SHA256 URL:  ${SHA256_URL}"

# ---------------------------------------------------------------------------
# Install location
# ---------------------------------------------------------------------------

if [ "${SYSTEM_INSTALL}" = "1" ] && [ -d "/usr/local/bin" ] && [ -w "/usr/local/bin" ]; then
    INSTALL_DIR="/usr/local/bin"
else
    INSTALL_DIR="${VECTORHAWK_HOME:-${HOME}/.vectorhawk}/bin"
fi

log_verbose "Install directory: ${INSTALL_DIR}"

# ---------------------------------------------------------------------------
# Idempotency check: is this version already installed?
# ---------------------------------------------------------------------------

VECTORHAWK_BIN="${INSTALL_DIR}/vectorhawk"

if [ -x "${VECTORHAWK_BIN}" ]; then
    _INSTALLED_VERSION="$("${VECTORHAWK_BIN}" --version 2>/dev/null | grep -o '[0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*' | head -1 || true)"
    if [ "${_INSTALLED_VERSION}" = "${VERSION}" ]; then
        log_info "vectorhawk ${VERSION} is already installed at ${INSTALL_DIR}."
        log_info ""
        log_info "Next: run \`vectorhawk daemon install\` to enable auto-start at login."
        log_info "Then: run \`vectorhawk mcp setup\` to configure your AI client."
        exit 0
    fi
fi

# ---------------------------------------------------------------------------
# Download to a tempdir, verify SHA256, then install
# ---------------------------------------------------------------------------

TMPDIR="$(mktemp -d)"
# shellcheck disable=SC2064
trap "rm -rf '${TMPDIR}'" EXIT INT TERM

log_info "Downloading vectorhawk ${VERSION} for ${TRIPLE}..."

download_url "${TARBALL_URL}" "${TMPDIR}/${TARBALL_NAME}"
download_url "${SHA256_URL}"  "${TMPDIR}/${SHA256_NAME}"

log_verbose "Verifying SHA256 checksum..."

# Verify checksum. Prefer sha256sum (Linux/GNU), fall back to shasum -a 256 (macOS).
# Both accept the GNU two-space format: "<64hex>  <filename>".
_CHECKSUM_OK=0

if command -v sha256sum >/dev/null 2>&1; then
    if (cd "${TMPDIR}" && sha256sum -c "${SHA256_NAME}" >/dev/null 2>&1); then
        _CHECKSUM_OK=1
    fi
else
    if (cd "${TMPDIR}" && shasum -a 256 -c "${SHA256_NAME}" >/dev/null 2>&1); then
        _CHECKSUM_OK=1
    fi
fi

if [ "${_CHECKSUM_OK}" = "0" ]; then
    _EXPECTED="$(cat "${TMPDIR}/${SHA256_NAME}" | awk '{print $1}')"
    if command -v sha256sum >/dev/null 2>&1; then
        _ACTUAL="$(sha256sum "${TMPDIR}/${TARBALL_NAME}" | awk '{print $1}')"
    else
        _ACTUAL="$(shasum -a 256 "${TMPDIR}/${TARBALL_NAME}" | awk '{print $1}')"
    fi
    log_error "SHA256 checksum mismatch for ${TARBALL_NAME}."
    log_error "  Expected: ${_EXPECTED}"
    log_error "  Actual:   ${_ACTUAL}"
    log_error "The download may be corrupt or tampered. Aborting."
    exit 1
fi

log_verbose "Checksum OK."

# ---------------------------------------------------------------------------
# Extract and install binaries
# ---------------------------------------------------------------------------

log_info "Extracting..."
tar -xzf "${TMPDIR}/${TARBALL_NAME}" -C "${TMPDIR}"

PKG_DIR="${TMPDIR}/vectorhawk-${VERSION}-${TRIPLE}"

# Create install directory if it does not exist.
mkdir -p "${INSTALL_DIR}"

log_verbose "Installing binaries to ${INSTALL_DIR}..."

for _bin in vectorhawk vectorhawkd vectorhawkd-shim; do
    _src="${PKG_DIR}/${_bin}"
    if [ -f "${_src}" ]; then
        # Atomic rename: copy then move so partial writes are not visible.
        cp "${_src}" "${INSTALL_DIR}/${_bin}.tmp"
        mv "${INSTALL_DIR}/${_bin}.tmp" "${INSTALL_DIR}/${_bin}"
        chmod 755 "${INSTALL_DIR}/${_bin}"
        log_verbose "Installed ${_bin}"
    else
        log_error "Expected binary not found in tarball: ${_bin}"
        exit 1
    fi
done

# ---------------------------------------------------------------------------
# PATH update
# ---------------------------------------------------------------------------

# Check if INSTALL_DIR is already on PATH.
_on_path=0
# Use a colon-delimited scan without relying on case/for with IFS manipulation.
_path_check=":${PATH}:"
case "${_path_check}" in
    *":${INSTALL_DIR}:"*) _on_path=1 ;;
esac

if [ "${_on_path}" = "1" ]; then
    log_verbose "${INSTALL_DIR} is already on PATH."
elif [ "${SYSTEM_INSTALL}" = "1" ]; then
    # System installs go to /usr/local/bin which is always on PATH.
    log_verbose "System install — PATH update not needed."
else
    _RC_FILE=""
    _PATH_LINE=""

    # Detect user's default shell.
    _SHELL_NAME="$(basename "${SHELL:-sh}")"
    case "${_SHELL_NAME}" in
        zsh)
            _RC_FILE="${HOME}/.zshrc"
            _PATH_LINE="export PATH=\"${INSTALL_DIR}:\$PATH\""
            ;;
        bash)
            _RC_FILE="${HOME}/.bashrc"
            _PATH_LINE="export PATH=\"${INSTALL_DIR}:\$PATH\""
            ;;
        fish)
            _RC_FILE="${HOME}/.config/fish/config.fish"
            _PATH_LINE="set -x PATH \"${INSTALL_DIR}\" \$PATH"
            ;;
        *)
            _RC_FILE=""
            _PATH_LINE=""
            ;;
    esac

    _MODIFY_PATH=0

    if [ "${NO_MODIFY_PATH}" = "1" ]; then
        _MODIFY_PATH=0
    elif [ -z "${_RC_FILE}" ]; then
        # Unknown shell — cannot determine rc file.
        _MODIFY_PATH=0
    elif [ ! -t 0 ]; then
        # Non-interactive (piped install) — do not modify rc files silently.
        _MODIFY_PATH=0
    else
        # Interactive: prompt the user.
        printf '\nAdd %s to PATH in %s? [y/N] ' "${INSTALL_DIR}" "${_RC_FILE}"
        read -r _REPLY </dev/tty
        case "${_REPLY}" in
            [Yy]|[Yy][Ee][Ss]) _MODIFY_PATH=1 ;;
            *) _MODIFY_PATH=0 ;;
        esac
    fi

    if [ "${_MODIFY_PATH}" = "1" ] && [ -n "${_RC_FILE}" ] && [ -n "${_PATH_LINE}" ]; then
        # Idempotency: do not double-write.
        _already_present=0
        if [ -f "${_RC_FILE}" ]; then
            if grep -qF "${INSTALL_DIR}" "${_RC_FILE}"; then
                _already_present=1
            fi
        fi
        if [ "${_already_present}" = "0" ]; then
            printf '\n# Added by VectorHawk installer\n%s\n' "${_PATH_LINE}" >> "${_RC_FILE}"
            log_info "Added PATH entry to ${_RC_FILE}."
        else
            log_verbose "${_RC_FILE} already contains ${INSTALL_DIR} — skipping."
        fi
    else
        # Print manual instructions.
        if [ -n "${_PATH_LINE}" ] && [ -n "${_RC_FILE}" ]; then
            log_info ""
            log_info "To add vectorhawk to your PATH, add this line to ${_RC_FILE}:"
            log_info ""
            log_info "    ${_PATH_LINE}"
            log_info ""
            log_info "Then run:  source ${_RC_FILE}"
        else
            log_info ""
            log_info "To add vectorhawk to your PATH, add this to your shell configuration:"
            log_info ""
            log_info "    export PATH=\"${INSTALL_DIR}:\$PATH\""
            log_info ""
        fi
    fi
fi

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------

log_info ""
log_info "vectorhawk ${VERSION} installed."
log_info "Next: run \`vectorhawk daemon install\` to enable auto-start at login."
log_info "Then: run \`vectorhawk mcp setup\` to configure your AI client."
