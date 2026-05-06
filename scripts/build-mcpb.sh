#!/usr/bin/env bash
# Build the vectorhawk binary and package it as a .mcpb Desktop Extension
# for Claude Desktop one-click install.
#
# Usage:
#   ./scripts/build-mcpb.sh [--target <rust-target-triple>] [--output-dir <dir>]
#
# Examples:
#   ./scripts/build-mcpb.sh
#   ./scripts/build-mcpb.sh --output-dir ./dist
#   ./scripts/build-mcpb.sh --target aarch64-apple-darwin --output-dir ./dist

set -euo pipefail

# ---------------------------------------------------------------------------
# Defaults
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

TARGET=""          # empty = native (no --target flag passed to cargo)
OUTPUT_DIR="$REPO_ROOT/dist"

PACKAGE_NAME="vectorhawk-runner"
DISPLAY_NAME="VectorHawk Runner"
VERSION="1.0.1"
DESCRIPTION="VectorHawk Runner — local AI skill runtime and MCP aggregator for portable skills, tool namespacing, and multi-client setup."
AUTHOR_NAME="VectorHawk"
AUTHOR_URL="https://vectorhawk.io"

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

while [[ $# -gt 0 ]]; do
  case "$1" in
    --target)
      TARGET="$2"
      shift 2
      ;;
    --output-dir)
      OUTPUT_DIR="$2"
      shift 2
      ;;
    *)
      echo "Unknown argument: $1" >&2
      echo "Usage: $0 [--target <rust-target-triple>] [--output-dir <dir>]" >&2
      exit 1
      ;;
  esac
done

# ---------------------------------------------------------------------------
# Derive platform string for the output filename
# ---------------------------------------------------------------------------

derive_platform() {
  local triple="${1:-}"

  if [[ -z "$triple" ]]; then
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
      Darwin)
        case "$arch" in
          arm64)  echo "darwin-arm64" ;;
          x86_64) echo "darwin-x64"   ;;
          *)      echo "darwin-$arch" ;;
        esac
        ;;
      Linux)
        case "$arch" in
          x86_64)          echo "linux-x64"   ;;
          aarch64|arm64)   echo "linux-arm64" ;;
          *)               echo "linux-$arch" ;;
        esac
        ;;
      MINGW*|MSYS*|CYGWIN*|Windows_NT)
        echo "win32-x64"
        ;;
      *)
        echo "unknown-$arch"
        ;;
    esac
  else
    case "$triple" in
      aarch64-apple-darwin)           echo "darwin-arm64"  ;;
      x86_64-apple-darwin)            echo "darwin-x64"    ;;
      x86_64-unknown-linux-gnu)       echo "linux-x64"     ;;
      x86_64-unknown-linux-musl)      echo "linux-x64"     ;;
      aarch64-unknown-linux-gnu)      echo "linux-arm64"   ;;
      aarch64-unknown-linux-musl)     echo "linux-arm64"   ;;
      x86_64-pc-windows-msvc)         echo "win32-x64"     ;;
      x86_64-pc-windows-gnu)          echo "win32-x64"     ;;
      *)                              echo "$triple"        ;;
    esac
  fi
}

PLATFORM="$(derive_platform "$TARGET")"

# ---------------------------------------------------------------------------
# Locate the binary after build
# ---------------------------------------------------------------------------

binary_path() {
  local target="${1:-}"
  if [[ -n "$target" ]]; then
    echo "$REPO_ROOT/target/$target/release/vectorhawk"
  else
    echo "$REPO_ROOT/target/release/vectorhawk"
  fi
}

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

echo "==> Building vectorhawk (release) for platform: $PLATFORM"

cd "$REPO_ROOT"

if [[ -n "$TARGET" ]]; then
  cargo build --release -p vectorhawkd-cli --target "$TARGET"
else
  cargo build --release -p vectorhawkd-cli
fi

BINARY="$(binary_path "$TARGET")"

if [[ ! -f "$BINARY" ]]; then
  echo "Error: expected binary not found at $BINARY" >&2
  exit 1
fi

echo "    Binary: $BINARY"

# ---------------------------------------------------------------------------
# Stage .mcpb contents in a temp directory
# ---------------------------------------------------------------------------

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

STAGE_DIR="$WORK_DIR/stage"
mkdir -p "$STAGE_DIR/bin"

echo "==> Staging .mcpb contents in $STAGE_DIR"

# 1. manifest.json — wire shape consumed by Claude Desktop's enterprise allowlist
cat > "$STAGE_DIR/manifest.json" <<JSON
{
  "schema_version": "1.0",
  "name": "$PACKAGE_NAME",
  "display_name": "$DISPLAY_NAME",
  "version": "$VERSION",
  "description": "$DESCRIPTION",
  "author": {
    "name": "$AUTHOR_NAME",
    "url": "$AUTHOR_URL"
  },
  "server": {
    "transport": "stdio",
    "command": "./bin/vectorhawk",
    "args": ["mcp", "serve"]
  }
}
JSON

# 2. Copy binary and ensure it is executable
cp "$BINARY" "$STAGE_DIR/bin/vectorhawk"
chmod +x "$STAGE_DIR/bin/vectorhawk"

# 3. Strip the binary to reduce size.
#    The release profile already sets `strip = true` via rustc, but an
#    explicit strip pass catches any remaining debug sections on platforms
#    where the Rust linker strip is incomplete.
if command -v strip &>/dev/null; then
  strip "$STAGE_DIR/bin/vectorhawk" 2>/dev/null || true
fi

# ---------------------------------------------------------------------------
# Create .mcpb archive (ZIP)
# ---------------------------------------------------------------------------

mkdir -p "$OUTPUT_DIR"
MCPB_NAME="${PACKAGE_NAME}-${VERSION}-${PLATFORM}.mcpb"
MCPB_PATH="$OUTPUT_DIR/$MCPB_NAME"

echo "==> Creating archive: $MCPB_NAME"

# Build the ZIP from inside the stage dir so paths inside the archive are
# relative (manifest.json at root, bin/vectorhawk inside bin/).
(
  cd "$STAGE_DIR"
  zip -r "$MCPB_PATH" .
)

# ---------------------------------------------------------------------------
# Verify and report
# ---------------------------------------------------------------------------

echo ""
echo "==> Verifying archive contents"
unzip -l "$MCPB_PATH"

echo ""
echo "==> Validating manifest.json structure"
MANIFEST_IN_ARCHIVE="$(unzip -p "$MCPB_PATH" manifest.json)"

for field in schema_version name display_name version description author server; do
  if ! echo "$MANIFEST_IN_ARCHIVE" | grep -q "\"$field\""; then
    echo "Error: manifest.json is missing field: $field" >&2
    exit 1
  fi
done
echo "    manifest.json OK"

# ---------------------------------------------------------------------------
# SHA-256 and size
# ---------------------------------------------------------------------------

echo ""
echo "==> Output"

if command -v sha256sum &>/dev/null; then
  SHA256="$(sha256sum "$MCPB_PATH" | awk '{print $1}')"
elif command -v shasum &>/dev/null; then
  SHA256="$(shasum -a 256 "$MCPB_PATH" | awk '{print $1}')"
else
  SHA256="(sha256sum/shasum not available)"
fi

SIZE_BYTES="$(wc -c < "$MCPB_PATH" | tr -d ' ')"
if [[ "$SIZE_BYTES" -ge 1048576 ]]; then
  SIZE_HUMAN="$(( SIZE_BYTES / 1048576 )) MB"
elif [[ "$SIZE_BYTES" -ge 1024 ]]; then
  SIZE_HUMAN="$(( SIZE_BYTES / 1024 )) KB"
else
  SIZE_HUMAN="${SIZE_BYTES} B"
fi

echo "    Path:   $MCPB_PATH"
echo "    Size:   $SIZE_HUMAN ($SIZE_BYTES bytes)"
echo "    SHA256: $SHA256"
echo ""
echo "Done."
