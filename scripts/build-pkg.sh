#!/usr/bin/env bash
#
# Build the macOS .pkg locally (same steps CI runs).
#
# Usage: scripts/build-pkg.sh [--flavor prod|dev] <version> <arm64-bin> <x86_64-bin> <out-dir>
#
# Writes <out-dir>/scenery-<version>-macos.pkg (or ...-macos-dev.pkg for the dev
# flavor). Signs only when SCENERY_PKG_SIGN=1 and APP_SIGN_ID / INSTALLER_SIGN_ID
# are set; otherwise builds unsigned.
set -euo pipefail
export COPYFILE_DISABLE=1   # no AppleDouble (._*) entries in the archive
umask 022   # payload dir modes must not depend on the builder's umask

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PKGSRC="$ROOT/packaging/macos"

FLAVOR=prod
if [[ "${1:-}" == "--flavor" ]]; then
  FLAVOR="${2:-}"; shift 2
fi
[[ "$FLAVOR" == "prod" || "$FLAVOR" == "dev" ]] || { echo "build-pkg: bad flavor '$FLAVOR'" >&2; exit 2; }
[[ $# -eq 4 ]] || { echo "usage: $0 [--flavor prod|dev] <version> <arm64-bin> <x86_64-bin> <out-dir>" >&2; exit 2; }
VERSION="$1"; ARM="$2"; X86="$3"; OUT="$4"

# macOS stamps com.apple.provenance on files we create, and the kernel keeps
# re-adding it (xattr -c cannot clear it), so pkgbuild archives zero-byte
# AppleDouble "._*" entries. Rewrite a component pkg without them.
scrub_appledouble() {
  local pkg="$1" tmp="$WORK/scrub.$$"
  # Leave pkgbuild's output untouched when there is nothing to scrub: look for
  # "._" entries in the Bom and in the Payload/Scripts archives.
  local xt="$WORK/check.$$" dirty=0 m
  rm -rf "$xt"; mkdir -p "$xt"
  /usr/bin/xar -xf "$pkg" -C "$xt"
  if [[ -f "$xt/Bom" ]] && /usr/bin/lsbom "$xt/Bom" | grep -Eq '(^|/)\._'; then
    dirty=1
  fi
  for m in Payload Scripts; do
    [[ -f "$xt/$m" ]] || continue
    if /usr/bin/gzip -dc < "$xt/$m" | /usr/bin/tar -tf - 2>/dev/null | grep -Eq '(^|/)\._'; then
      dirty=1
    fi
  done
  rm -rf "$xt"
  [[ "$dirty" == 1 ]] || return 0
  rm -rf "$tmp"
  mkdir -p "$tmp"
  /usr/bin/xar -xf "$pkg" -C "$tmp"
  local f
  for f in Payload Scripts; do
    [[ -f "$tmp/$f" ]] || continue
    /usr/bin/gzip -dc < "$tmp/$f" | /usr/bin/tar --format odc -cf - --exclude '._*' @- \
      | /usr/bin/gzip -c > "$tmp/$f.new"
    mv "$tmp/$f.new" "$tmp/$f"
  done
  if [[ -f "$tmp/Bom" ]]; then
    /usr/bin/lsbom "$tmp/Bom" | grep -v -E '(^|/)\._' > "$tmp/bom.list"
    /usr/bin/mkbom -i "$tmp/bom.list" "$tmp/Bom.new"
    mv "$tmp/Bom.new" "$tmp/Bom"; rm -f "$tmp/bom.list"
  fi
  rm -f "$pkg"
  local members=()
  for f in Bom PackageInfo Payload Scripts; do
    [[ -e "$tmp/$f" ]] && members+=("$f")
  done
  ( cd "$tmp" && /usr/bin/xar -cf "$pkg" --compression none "${members[@]}" )
  rm -rf "$tmp"
}

SIGN=0
if [[ "${SCENERY_PKG_SIGN:-}" == "1" && -n "${APP_SIGN_ID:-}" && -n "${INSTALLER_SIGN_ID:-}" ]]; then
  SIGN=1
else
  echo "pkg: unsigned (no Apple secrets)"
fi

mkdir -p "$OUT"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/scenery-pkg.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
ROOTDIR="$WORK/root"; PKGS="$WORK/pkgs"; SCRIPTS="$WORK/scripts"
SUPPORT="$ROOTDIR/Library/Application Support/Scenery"
mkdir -p "$SUPPORT/bin" "$ROOTDIR/Library/LaunchAgents" "$PKGS"
chmod 0755 "$ROOTDIR" "$ROOTDIR/Library" "$ROOTDIR/Library/LaunchAgents" "$ROOTDIR/Library/Application Support" "$SUPPORT" "$SUPPORT/bin"

echo "pkg: lipo universal binary"
ARM_ARCHS="$(/usr/bin/lipo -archs "$ARM")"
X86_ARCHS="$(/usr/bin/lipo -archs "$X86")"
if [[ "$ARM_ARCHS" == "$X86_ARCHS" && "$(cmp -s "$ARM" "$X86" && echo same)" == "same" ]]; then
  # Identical inputs (tests use copies of one binary): nothing to merge.
  cp "$ARM" "$SUPPORT/bin/scenery"
else
  [[ "$ARM_ARCHS" == *arm64* ]] || { echo "build-pkg: $ARM is not arm64 ($ARM_ARCHS)" >&2; exit 1; }
  [[ "$X86_ARCHS" == *x86_64* ]] || { echo "build-pkg: $X86 is not x86_64 ($X86_ARCHS)" >&2; exit 1; }
  /usr/bin/lipo -create "$ARM" "$X86" -output "$SUPPORT/bin/scenery"
  OUT_ARCHS="$(/usr/bin/lipo -archs "$SUPPORT/bin/scenery")"
  [[ "$OUT_ARCHS" == *arm64* && "$OUT_ARCHS" == *x86_64* ]] \
    || { echo "build-pkg: merged binary lacks an arch ($OUT_ARCHS)" >&2; exit 1; }
fi
chmod 0755 "$SUPPORT/bin/scenery"
if [[ "$SIGN" == 1 ]]; then
  /usr/bin/codesign --force --options runtime --timestamp --sign "$APP_SIGN_ID" "$SUPPORT/bin/scenery"
fi
install -m 0755 "$PKGSRC/uninstall.sh" "$SUPPORT/uninstall.sh"
install -m 0644 "$PKGSRC/dev.scenery.agent.plist" "$ROOTDIR/Library/LaunchAgents/dev.scenery.agent.plist"

/usr/bin/xattr -cr "$ROOTDIR" 2>/dev/null || true
mkdir -p "$SCRIPTS"
install -m 0755 "$PKGSRC/scripts/preinstall" "$SCRIPTS/preinstall"
sed "s/^SCENERY_PKG_FLAVOR=.*/SCENERY_PKG_FLAVOR=$FLAVOR/" "$PKGSRC/scripts/postinstall" > "$SCRIPTS/postinstall"
chmod 0755 "$SCRIPTS/postinstall"

echo "pkg: pkgbuild component ($FLAVOR)"
/usr/bin/pkgbuild --compression legacy --root "$ROOTDIR" --identifier dev.scenery.runner --version "$VERSION" \
  --install-location / --ownership recommended --scripts "$SCRIPTS" "$PKGS/scenery-component.pkg"
scrub_appledouble "$PKGS/scenery-component.pkg"

DIST="$PKGSRC/distribution.xml"
SUFFIX=""
if [[ "$FLAVOR" == "dev" ]]; then
  DIST="$PKGSRC/distribution-dev.xml"; SUFFIX="-dev"
  DEVROOT="$WORK/devroot/Library/Application Support/Scenery"
  mkdir -p "$DEVROOT"
  printf 'https://dev.scenery.dev\n' > "$DEVROOT/registry-url"
  chmod 0644 "$DEVROOT/registry-url"
  /usr/bin/xattr -cr "$WORK/devroot" 2>/dev/null || true
  /usr/bin/pkgbuild --compression legacy --root "$WORK/devroot" --identifier dev.scenery.runner.devregistry --version "$VERSION" \
    --install-location / --ownership recommended "$PKGS/scenery-devregistry.pkg"
  scrub_appledouble "$PKGS/scenery-devregistry.pkg"
  /usr/bin/pkgbuild --compression legacy --nopayload --identifier dev.scenery.runner.prodregistry --version "$VERSION" \
    --scripts "$PKGSRC/scripts-prodchoice" "$PKGS/scenery-prodregistry.pkg"
  scrub_appledouble "$PKGS/scenery-prodregistry.pkg"
fi

sed "s/@VERSION@/$VERSION/g" "$DIST" > "$WORK/distribution.xml"
FINAL="$OUT/scenery-$VERSION-macos$SUFFIX.pkg"
rm -f "$FINAL"
PB=(/usr/bin/productbuild --distribution "$WORK/distribution.xml" --package-path "$PKGS")
[[ "$SIGN" == 1 ]] && PB+=(--sign "$INSTALLER_SIGN_ID")
"${PB[@]}" "$FINAL"
echo "pkg: wrote $FINAL"
