#!/usr/bin/env bash
#
# Tests scripts/build-pkg.sh by building the pkg with fake binaries and
# inspecting it. Never installs anything: no installer, sudo or launchctl.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/target/pkg-test"
VER="9.9.9"
PASS=0; FAIL=0

ok()   { echo "PASS: $1"; PASS=$((PASS + 1)); }
bad()  { echo "FAIL: $1"; FAIL=$((FAIL + 1)); }
check() { # check <description> <command...>
  local d="$1"; shift
  if "$@" >/dev/null 2>&1; then ok "$d"; else bad "$d"; fi
}

cleanup() { rm -rf "$OUT/expand-prod" "$OUT/expand-dev"; }
trap cleanup EXIT

rm -rf "$OUT"; mkdir -p "$OUT"
cp /usr/bin/true "$OUT/fake-arm64"
cp /usr/bin/true "$OUT/fake-x86_64"

PROD="$OUT/scenery-$VER-macos.pkg"
DEV="$OUT/scenery-$VER-macos-dev.pkg"

"$ROOT/scripts/build-pkg.sh" "$VER" "$OUT/fake-arm64" "$OUT/fake-x86_64" "$OUT" >"$OUT/build-prod.log" 2>&1 \
  || { cat "$OUT/build-prod.log"; bad "prod build"; echo "FAILED"; exit 1; }
"$ROOT/scripts/build-pkg.sh" --flavor dev "$VER" "$OUT/fake-arm64" "$OUT/fake-x86_64" "$OUT" >"$OUT/build-dev.log" 2>&1 \
  || { cat "$OUT/build-dev.log"; bad "dev build"; echo "FAILED"; exit 1; }

check "prod pkg exists" test -f "$PROD"
check "dev pkg exists" test -f "$DEV"

FILES="$(/usr/sbin/pkgutil --payload-files "$PROD")"
for f in "./Library/Application Support/Scenery/bin/scenery" \
         "./Library/Application Support/Scenery/uninstall.sh" \
         "./Library/LaunchAgents/dev.scenery.agent.plist"; do
  check "payload has $f" grep -qxF "$f" <<<"$FILES"
done

check "no AppleDouble (._*) payload entries" test -z "$(grep -E '(^|/)\._' <<<"$FILES")"
/usr/sbin/pkgutil --expand "$PROD" "$OUT/expand-prod"
/usr/sbin/pkgutil --expand "$DEV" "$OUT/expand-dev"
COMP="$OUT/expand-prod/scenery-component.pkg"
for s in preinstall postinstall; do
  check "prod Scripts/$s executable" test -x "$COMP/Scripts/$s"
done
check "package identifier dev.scenery.runner" grep -q 'identifier="dev.scenery.runner"' "$COMP/PackageInfo"

check "uninstall.sh re-enables com.scenery.agent" grep -q 'launchctl" enable\|launchctl enable' "$ROOT/packaging/macos/uninstall.sh"
check "uninstall.sh re-enables brew agent" grep -q 'homebrew.mxcl.scenery' "$ROOT/packaging/macos/uninstall.sh"
check "uninstall.sh enables per local uid" grep -q 'dscl . -list /Users UniqueID' "$ROOT/packaging/macos/uninstall.sh"
check "postinstall retries bootstrap once" grep -q '/bin/sleep 1' "$ROOT/packaging/macos/scripts/postinstall"
check "pkgbuild pins --compression legacy" test "$(grep -c 'pkgbuild --compression legacy' "$ROOT/scripts/build-pkg.sh")" = 3

PLIST="$ROOT/packaging/macos/dev.scenery.agent.plist"
check "plutil -lint plist" /usr/bin/plutil -lint "$PLIST"
LABEL="$(/usr/libexec/PlistBuddy -c 'Print :Label' "$PLIST")"
check "plist Label is dev.scenery.agent" test "$LABEL" = "dev.scenery.agent"

if command -v shellcheck >/dev/null 2>&1; then
  check "shellcheck clean" shellcheck "$ROOT/packaging/macos/scripts/preinstall" \
    "$ROOT/packaging/macos/scripts/postinstall" "$ROOT/packaging/macos/uninstall.sh" \
    "$ROOT/packaging/macos/scripts-prodchoice/postinstall" \
    "$ROOT/scripts/build-pkg.sh" "$ROOT/scripts/test_build_pkg.sh"
else
  echo "SKIP: shellcheck not installed"
fi

# Prod flavor: no dev registry component.
check "prod has no devregistry" test ! -e "$OUT/expand-prod/scenery-devregistry.pkg"

# Dev flavor.
DEVC="$OUT/expand-dev/scenery-devregistry.pkg"
check "dev has devregistry component" test -d "$DEVC"
check "devregistry identifier" grep -q 'identifier="dev.scenery.runner.devregistry"' "$DEVC/PackageInfo"
mkdir -p "$OUT/devpayload"
( cd "$OUT/devpayload" && /usr/bin/gzip -dc < "$DEVC/Payload" | /usr/bin/cpio -id 2>/dev/null )
RU="$OUT/devpayload/Library/Application Support/Scenery/registry-url"
check "devregistry payload has registry-url" test -f "$RU"
check "registry-url content" test "$(tr -d '\n' < "$RU")" = "https://dev.scenery.dev"
DD="$OUT/expand-dev/Distribution"
check "dev Distribution is well-formed XML" xmllint --noout "$DD"
check "dev Distribution derives prod from dev" grep -qF "selected=\"!choices['dev'].selected\"" "$DD"
check "dev Distribution: prod choice visible" grep -q '<choice id="prod"[^>]*visible="true"' "$DD"
check "dev Distribution: dev choice visible" grep -q '<choice id="dev"[^>]*visible="true"' "$DD"
check "dev Distribution: no exclusive-parent attribute" test -z "$(grep -o 'selection="exclusive"' "$DD")"
check "runner choice always on" grep -q '<choice id="runner"[^>]*enabled="false"' "$DD"
rm -rf "$OUT/devpayload"

check "dev flavor has no AppleDouble entries" test -z "$(find "$OUT/expand-dev" -name '._*')"
check "no AppleDouble in expanded prod pkg" test -z "$(find "$OUT/expand-prod" -name '._*')"
check "no AppleDouble in raw component xar" test -z "$(/usr/bin/xar -tf "$PROD" | grep -E '(^|/)\._')"

# Directory modes in the payload must be 0755 regardless of the builder's umask.
BOMLIST="$(/usr/bin/lsbom -s -p fM "$COMP/Bom" 2>/dev/null || true)"
check "Bom lists payload" test -n "$BOMLIST"
check "payload dir modes are 0755" test -z "$(/usr/bin/lsbom -d -p fm "$COMP/Bom" | awk -F'\t' '$NF != "40755" {print}')"

# Arch handling: real different-arch inputs merge; a corrupt x86 input must fail.
if command -v clang >/dev/null 2>&1; then
  echo 'int main(void){return 0;}' > "$OUT/t.c"
  if clang -arch arm64 "$OUT/t.c" -o "$OUT/real-arm64" 2>/dev/null && clang -arch x86_64 "$OUT/t.c" -o "$OUT/real-x86" 2>/dev/null; then
    check "different-arch inputs build" "$ROOT/scripts/build-pkg.sh" "$VER" "$OUT/real-arm64" "$OUT/real-x86" "$OUT/arch"
    /usr/sbin/pkgutil --expand "$OUT/arch/scenery-$VER-macos.pkg" "$OUT/expand-arch"
    mkdir -p "$OUT/archpayload"
    ( cd "$OUT/archpayload" && /usr/bin/gzip -dc < "$OUT/expand-arch/scenery-component.pkg/Payload" | /usr/bin/cpio -id 2>/dev/null )
    ARCHS="$(/usr/bin/lipo -archs "$OUT/archpayload/Library/Application Support/Scenery/bin/scenery")"
    check "shipped binary has arm64 and x86_64 (got: $ARCHS)" test "$ARCHS" = "x86_64 arm64" -o "$ARCHS" = "arm64 x86_64"
    echo 'garbage' > "$OUT/corrupt-x86"
    # shellcheck disable=SC2016
    check "corrupt x86 input fails the build" bash -c '! "$0" "$1" "$2" "$3" "$4" >/dev/null 2>&1' "$ROOT/scripts/build-pkg.sh" "$VER" "$OUT/real-arm64" "$OUT/corrupt-x86" "$OUT/arch2"
    rm -rf "$OUT/expand-arch" "$OUT/archpayload"
  else
    echo "SKIP: cannot compile per-arch test binaries"
  fi
else
  echo "SKIP: clang not available for arch tests"
fi

echo "---- $PASS passed, $FAIL failed"
[[ "$FAIL" == 0 ]]
