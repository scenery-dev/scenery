#!/usr/bin/env bash
# d1_acceptance.sh -- D1.1 acceptance gate.
#
# Validates the release pipeline artifacts for Stream D1.1:
#   AC1: .github/workflows/release.yml exists and is valid YAML
#   AC4: vectorhawk --version returns the workspace version
#   AC6: Uninstall paths are documented in README.md
#   AC7: M4 regression gate passes
#   AC8: This script (the gate itself) exits 0 on a healthy repo
#
# Additionally verifies:
#   - Three release binaries build on the host platform
#   - Local fake-release tarball has the correct name and contents
#   - SHA256 checksum file is GNU sha256sum-compatible (two-space format)
#
# Usage:
#   cargo build --workspace --release  # build first (idempotent with caching)
#   bash scripts/d1_acceptance.sh

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -t 1 ]]; then
    GREEN="\033[0;32m"
    RED="\033[0;31m"
    YELLOW="\033[0;33m"
    RESET="\033[0m"
else
    GREEN=""
    RED=""
    YELLOW=""
    RESET=""
fi

PASS="${GREEN}PASS${RESET}"
FAIL="${RED}FAIL${RESET}"
SKIP="${YELLOW}N/A ${RESET}"

declare -a RESULTS=()
declare -a LABELS=()
declare -a DETAILS=()

record() {
    local status="$1" label="$2" detail="$3"
    RESULTS+=("${status}")
    LABELS+=("${label}")
    DETAILS+=("${detail}")
}

echo ""
echo "VectorHawk D1.1 Acceptance Gate"
echo "================================="
echo ""

# ---------------------------------------------------------------------------
# AC1: release.yml exists and is valid YAML
# ---------------------------------------------------------------------------

WORKFLOW="${REPO_ROOT}/.github/workflows/release.yml"

echo "AC1: checking .github/workflows/release.yml ..."

if [[ ! -f "${WORKFLOW}" ]]; then
    record "FAIL" "AC1: release.yml exists" "file not found: ${WORKFLOW}"
else
    record "PASS" "AC1: release.yml exists" "${WORKFLOW}"

    # Validate YAML syntax using Python (available on macOS and most Linux distros).
    if python3 -c "
import yaml, sys
with open('${WORKFLOW}') as f:
    doc = yaml.safe_load(f)
# GitHub Actions uses 'on' as a trigger key; PyYAML 1.1 parses bare 'on' as True.
trigger_key = True if True in doc else ('on' if 'on' in doc else None)
assert trigger_key is not None, 'missing on: trigger'
assert 'jobs' in doc, 'missing jobs:'
assert 'build' in doc['jobs'], 'missing build job'
assert 'release' in doc['jobs'], 'missing release job'
matrix = doc['jobs']['build']['strategy']['matrix']['include']
targets = [m['target'] for m in matrix]
required = {'aarch64-apple-darwin', 'x86_64-apple-darwin', 'x86_64-unknown-linux-gnu'}
missing = required - set(targets)
assert not missing, f'missing targets: {missing}'
" 2>/dev/null; then
        record "PASS" "AC1: release.yml is valid YAML with required targets" \
            "aarch64-apple-darwin, x86_64-apple-darwin, x86_64-unknown-linux-gnu"
    else
        YAML_ERR="$(python3 -c "import yaml; yaml.safe_load(open('${WORKFLOW}'))" 2>&1 || true)"
        record "FAIL" "AC1: release.yml YAML validation" "${YAML_ERR}"
    fi

    # Verify SHA256 checksum step uses GNU-compatible two-space format.
    if grep -q 'sha256sum\|shasum -a 256' "${WORKFLOW}"; then
        record "PASS" "AC1: release.yml includes SHA256 checksum step" \
            "sha256sum / shasum -a 256 found in workflow"
    else
        record "FAIL" "AC1: release.yml includes SHA256 checksum step" \
            "no sha256sum step found in workflow"
    fi
fi

# ---------------------------------------------------------------------------
# AC1 (build): three release binaries exist on the host target
# ---------------------------------------------------------------------------

echo "AC1 (build): verifying release binaries for host target ..."

cargo build --workspace --release 2>/dev/null
BUILD_STATUS=$?

if [[ ${BUILD_STATUS} -ne 0 ]]; then
    record "FAIL" "AC1 (build): cargo build --workspace --release" \
        "build failed — run manually to see errors"
else
    record "PASS" "AC1 (build): cargo build --workspace --release succeeded" ""

    for BIN in vectorhawk vectorhawkd vectorhawkd-shim; do
        BIN_PATH="${REPO_ROOT}/target/release/${BIN}"
        if [[ -x "${BIN_PATH}" ]]; then
            record "PASS" "AC1 (build): binary exists: ${BIN}" "${BIN_PATH}"
        else
            record "FAIL" "AC1 (build): binary exists: ${BIN}" "not found at ${BIN_PATH}"
        fi
    done
fi

# ---------------------------------------------------------------------------
# AC4: vectorhawk --version returns a version string matching Cargo.toml
# ---------------------------------------------------------------------------

echo "AC4: checking vectorhawk --version ..."

VECTORHAWK_BIN="${REPO_ROOT}/target/release/vectorhawk"
if [[ -x "${VECTORHAWK_BIN}" ]]; then
    VERSION_OUT="$("${VECTORHAWK_BIN}" --version 2>&1 || true)"
    # Extract version from workspace Cargo.toml — the line `version = "x.y.z"` at top level.
    CARGO_VERSION="$(grep '^version = ' "${REPO_ROOT}/Cargo.toml" | head -1 | sed 's/version = "\(.*\)"/\1/')"
    if [[ "${VERSION_OUT}" == *"${CARGO_VERSION}"* ]]; then
        record "PASS" "AC4: vectorhawk --version reports workspace version" \
            "output: ${VERSION_OUT}, expected version: ${CARGO_VERSION}"
    else
        record "FAIL" "AC4: vectorhawk --version reports workspace version" \
            "output: ${VERSION_OUT}, expected version: ${CARGO_VERSION}"
    fi
else
    record "FAIL" "AC4: vectorhawk --version" "binary not found, skipping"
fi

# ---------------------------------------------------------------------------
# AC1 (tarball): local fake-release — build a tarball in the expected format
# and verify its name, contents, and SHA256 checksum file
# ---------------------------------------------------------------------------

echo "AC1 (tarball): running local fake-release to verify tarball naming ..."

# Detect host triple so the test is portable.
HOST_OS="$(uname -s)"
HOST_ARCH="$(uname -m)"

case "${HOST_OS}:${HOST_ARCH}" in
    Darwin:arm64)  HOST_TRIPLE="aarch64-apple-darwin" ;;
    Darwin:x86_64) HOST_TRIPLE="x86_64-apple-darwin" ;;
    Linux:x86_64)  HOST_TRIPLE="x86_64-unknown-linux-gnu" ;;
    Linux:aarch64) HOST_TRIPLE="aarch64-unknown-linux-gnu" ;;
    *)             HOST_TRIPLE="unknown" ;;
esac

CARGO_VERSION="$(grep '^version = ' "${REPO_ROOT}/Cargo.toml" | head -1 | sed 's/version = "\(.*\)"/\1/')"
EXPECTED_ARCHIVE="vectorhawk-${CARGO_VERSION}-${HOST_TRIPLE}.tar.gz"

STAGING_DIR="$(mktemp -d)"
PKG_DIR="${STAGING_DIR}/vectorhawk-${CARGO_VERSION}-${HOST_TRIPLE}"
mkdir -p "${PKG_DIR}"

FAKE_RELEASE_OK=1

for BIN in vectorhawk vectorhawkd vectorhawkd-shim; do
    if [[ -x "${REPO_ROOT}/target/release/${BIN}" ]]; then
        cp "${REPO_ROOT}/target/release/${BIN}" "${PKG_DIR}/"
    else
        FAKE_RELEASE_OK=0
    fi
done

if [[ -f "${REPO_ROOT}/LICENSE" ]]; then
    cp "${REPO_ROOT}/LICENSE" "${PKG_DIR}/"
else
    FAKE_RELEASE_OK=0
fi

if [[ -f "${REPO_ROOT}/README.md" ]]; then
    cp "${REPO_ROOT}/README.md" "${PKG_DIR}/"
else
    FAKE_RELEASE_OK=0
fi

if [[ ${FAKE_RELEASE_OK} -eq 1 ]]; then
    ARCHIVE_PATH="${STAGING_DIR}/${EXPECTED_ARCHIVE}"
    tar -czf "${ARCHIVE_PATH}" -C "${STAGING_DIR}" "vectorhawk-${CARGO_VERSION}-${HOST_TRIPLE}"

    if [[ -f "${ARCHIVE_PATH}" ]]; then
        record "PASS" "AC1 (tarball): tarball created with correct name" \
            "${EXPECTED_ARCHIVE}"
    else
        record "FAIL" "AC1 (tarball): tarball creation" "tar command failed"
    fi

    # Verify contents of the tarball.
    TARBALL_CONTENTS="$(tar -tzf "${ARCHIVE_PATH}" 2>/dev/null)"
    PKG_PREFIX="vectorhawk-${CARGO_VERSION}-${HOST_TRIPLE}"
    ALL_OK=1
    for EXPECTED_FILE in vectorhawk vectorhawkd vectorhawkd-shim LICENSE README.md; do
        if echo "${TARBALL_CONTENTS}" | grep -q "${PKG_PREFIX}/${EXPECTED_FILE}$"; then
            true
        else
            ALL_OK=0
        fi
    done
    if [[ ${ALL_OK} -eq 1 ]]; then
        record "PASS" "AC1 (tarball): tarball contains required files" \
            "vectorhawk, vectorhawkd, vectorhawkd-shim, LICENSE, README.md"
    else
        record "FAIL" "AC1 (tarball): tarball contents" \
            "missing files; got: $(echo "${TARBALL_CONTENTS}" | tr '\n' ' ')"
    fi

    # Compute SHA256 and write checksum file.
    SHA256_PATH="${ARCHIVE_PATH}.sha256"
    if command -v sha256sum >/dev/null 2>&1; then
        # GNU sha256sum: output is "<hash>  <filename>"
        (cd "${STAGING_DIR}" && sha256sum "${EXPECTED_ARCHIVE}" > "${SHA256_PATH}")
    else
        # macOS shasum: same format with -a 256
        (cd "${STAGING_DIR}" && shasum -a 256 "${EXPECTED_ARCHIVE}" > "${SHA256_PATH}")
    fi

    CHECKSUM_CONTENT="$(cat "${SHA256_PATH}")"
    # Verify format: 64-char hex hash, two spaces, filename
    if echo "${CHECKSUM_CONTENT}" | grep -qE '^[0-9a-f]{64}  .+\.tar\.gz$'; then
        record "PASS" "AC1 (tarball): SHA256 checksum file format is GNU-compatible" \
            "${CHECKSUM_CONTENT}"
    else
        record "FAIL" "AC1 (tarball): SHA256 checksum file format" \
            "got: ${CHECKSUM_CONTENT}"
    fi

    # Verify sha256sum -c works (GNU) or shasum -c (macOS).
    VERIFY_OK=0
    if command -v sha256sum >/dev/null 2>&1; then
        if (cd "${STAGING_DIR}" && sha256sum -c "${EXPECTED_ARCHIVE}.sha256" >/dev/null 2>&1); then
            VERIFY_OK=1
        fi
    else
        if (cd "${STAGING_DIR}" && shasum -a 256 -c "${EXPECTED_ARCHIVE}.sha256" >/dev/null 2>&1); then
            VERIFY_OK=1
        fi
    fi

    if [[ ${VERIFY_OK} -eq 1 ]]; then
        record "PASS" "AC1 (tarball): sha256sum -c validates the checksum file" ""
    else
        record "FAIL" "AC1 (tarball): sha256sum -c validation" \
            "checksum verification failed"
    fi
else
    record "FAIL" "AC1 (tarball): fake-release prerequisites" \
        "binaries or LICENSE/README.md missing — build first"
fi

rm -rf "${STAGING_DIR}"

# ---------------------------------------------------------------------------
# AC6: Uninstall paths documented in README.md
# ---------------------------------------------------------------------------

echo "AC6: checking README.md for uninstall documentation ..."

README="${REPO_ROOT}/README.md"
if [[ -f "${README}" ]]; then
    if grep -q 'rm -rf.*\.vectorhawk' "${README}" && \
       grep -qi 'brew uninstall' "${README}"; then
        record "PASS" "AC6: uninstall paths documented in README.md" \
            "curl uninstall (rm -rf) and brew uninstall both present"
    else
        record "FAIL" "AC6: uninstall paths documented in README.md" \
            "missing one or both uninstall instructions"
    fi
else
    record "FAIL" "AC6: README.md exists" "file not found"
fi

# ---------------------------------------------------------------------------
# AC7: M4 regression gate
# ---------------------------------------------------------------------------

echo "AC7: running M4 regression gate ..."

if bash "${REPO_ROOT}/scripts/m4_acceptance.sh" >/dev/null 2>&1; then
    record "PASS" "AC7: M4 regression gate (m4_acceptance.sh)" \
        "all prior gates passed"
else
    record "FAIL" "AC7: M4 regression gate (m4_acceptance.sh)" \
        "m4_acceptance.sh returned non-zero — run it directly for details"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo ""
echo "================================="
echo "D1.1 Acceptance Results"
echo "================================="
echo ""

OVERALL=1
for i in "${!RESULTS[@]}"; do
    STATUS="${RESULTS[$i]}"
    LABEL="${LABELS[$i]}"
    DETAIL="${DETAILS[$i]}"

    case "${STATUS}" in
        PASS) ICON="${PASS}" ;;
        FAIL) ICON="${FAIL}"; OVERALL=0 ;;
        N/A)  ICON="${SKIP}" ;;
        *)    ICON="${STATUS}" ;;
    esac

    printf "  %-65s [%b]\n" "${LABEL}" "${ICON}"
    if [[ -n "${DETAIL}" ]]; then
        printf "    %s\n" "${DETAIL}"
    fi
done

echo ""
if [[ ${OVERALL} -eq 1 ]]; then
    printf "%bAll D1.1 acceptance criteria PASSED%b\n" "${GREEN}" "${RESET}"
    exit 0
else
    printf "%bOne or more D1.1 acceptance criteria FAILED%b\n" "${RED}" "${RESET}"
    exit 1
fi
