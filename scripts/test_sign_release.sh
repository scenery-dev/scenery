#!/bin/bash
# Tests sign-release.sh with stubbed gh and minisign on PATH: with and without
# the macOS pkg, a bad pkg name, and a checksum mismatch. Never touches GitHub.
# Usage: scripts/test_sign_release.sh
set -u
SRC="$(cd "$(dirname "$0")" && pwd)/sign-release.sh"
ROOT="$(mktemp -d)"; trap 'rm -rf "$ROOT"' EXIT
PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "ok   - $1"; }
bad()  { FAIL=$((FAIL+1)); echo "FAIL - $1"; }
check(){ if eval "$2"; then ok "$1"; else bad "$1"; fi; }
sha() { shasum -a 256 "$1" | awk '{print $1}'; }

BIN="$ROOT/bin"; mkdir -p "$BIN"
cat > "$BIN/gh" <<'S'
#!/bin/bash
echo "GH $*" >> "$CALLS"
case "$1 $2" in
  "release view") echo true ;;
  "release download")
    while [ $# -gt 0 ]; do [ "$1" = "--dir" ] && d="$2"; shift; done
    cp "$FIX"/* "$d"/ ;;
  "attestation verify") ;;
esac
exit 0
S
cat > "$BIN/minisign" <<'S'
#!/bin/bash
echo "MINISIGN $*" >> "$CALLS"
while [ $# -gt 0 ]; do [ "$1" = "-x" ] && x="$2"; shift; done
echo sig > "$x"
exit 0
S
chmod +x "$BIN"/*
touch "$ROOT/fake.key"
export PATH="$BIN:$PATH" CALLS="$ROOT/calls.log" SCENERY_MINISIGN_KEY="$ROOT/fake.key"

# fixture <dir> <pkgname|""> : two tarballs, the two scan scripts, optional
# pkg, matching SHA256SUMS
fixture() {
  FIX="$ROOT/$1"; rm -rf "$FIX"; mkdir -p "$FIX"; export FIX
  echo a > "$FIX/scenery-9.9.9-aarch64-apple-darwin.tar.gz"
  echo b > "$FIX/scenery-9.9.9-x86_64-apple-darwin.tar.gz"
  echo s > "$FIX/scan.sh"
  echo m > "$FIX/scan-mdm.sh"
  [ -n "$2" ] && echo p > "$FIX/$2"
  ( cd "$FIX" && for f in *; do echo "$(sha "$f")  $f"; done > "$ROOT/sums.tmp" ) && mv "$ROOT/sums.tmp" "$FIX/SHA256SUMS"
}
run() { : > "$CALLS"; bash "$SRC" v9.9.9 >"$ROOT/out.log" 2>&1; }

fixture f1 ""
run; rc=$?
check "no pkg: succeeds" "[ $rc = 0 ]"
check "no pkg: signs and publishes" "grep -q 'release edit.*--draft=false' $CALLS && grep -q MINISIGN $CALLS"
check "no pkg: attests tarballs, scan scripts + sums" "[ \$(grep -c 'attestation verify' $CALLS) = 5 ] && grep -q 'attestation verify.*/scan-mdm.sh' $CALLS"

fixture f2 scenery-9.9.9-macos.pkg
run; rc=$?
check "with pkg: succeeds" "[ $rc = 0 ]"
check "with pkg: attests pkg too" "[ \$(grep -c 'attestation verify' $CALLS) = 6 ] && grep -q 'attestation verify.*macos.pkg' $CALLS"
check "with pkg: published" "grep -q 'release edit.*--draft=false' $CALLS"

fixture f3 scenery-9.9.9-macos-dev.pkg
run; rc=$?
check "wrong pkg name: refused" "[ $rc != 0 ] && grep -q 'expected only scenery-9.9.9-macos.pkg' $ROOT/out.log"
check "wrong pkg name: nothing published" "! grep -q 'release edit' $CALLS"

fixture f4 scenery-9.9.9-macos.pkg
echo tampered > "$FIX/scenery-9.9.9-macos.pkg"
run; rc=$?
check "pkg checksum mismatch: refused" "[ $rc != 0 ] && grep -q 'checksum mismatch' $ROOT/out.log"
check "pkg checksum mismatch: nothing published" "! grep -q 'release edit' $CALLS"

fixture f5 scenery-9.9.9-macos.pkg
sed -i.bak '/macos.pkg/d' "$FIX/SHA256SUMS"; rm -f "$FIX/SHA256SUMS.bak"
run; rc=$?
check "pkg missing from SHA256SUMS: refused" "[ $rc != 0 ] && grep -q 'missing from SHA256SUMS' $ROOT/out.log"

fixture f6 ""
rm -f "$FIX/scan-mdm.sh"; sed -i.bak '/scan-mdm.sh/d' "$FIX/SHA256SUMS"; rm -f "$FIX/SHA256SUMS.bak"
run; rc=$?
check "scan-mdm.sh missing from draft: refused" "[ $rc != 0 ] && grep -q 'scan-mdm.sh' $ROOT/out.log"
check "scan-mdm.sh missing from draft: nothing published" "! grep -q 'release edit' $CALLS"

fixture f7 ""
echo tampered > "$FIX/scan.sh"
run; rc=$?
check "scan.sh checksum mismatch: refused" "[ $rc != 0 ] && grep -q 'checksum mismatch for scan.sh' $ROOT/out.log"
check "scan.sh checksum mismatch: nothing published" "! grep -q 'release edit' $CALLS"

fixture f8 ""
sed -i.bak '/ scan.sh$/d' "$FIX/SHA256SUMS"; rm -f "$FIX/SHA256SUMS.bak"
run; rc=$?
check "scan.sh missing from SHA256SUMS: refused" "[ $rc != 0 ] && grep -q 'scan.sh missing from SHA256SUMS' $ROOT/out.log"

echo "---- $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
