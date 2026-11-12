#!/bin/bash
# Harness for release.sh: stubs gh / sign / stage via REL_* env vars, uses
# throwaway git repos with bare origins. Never touches GitHub or a real host.
# Usage: scripts/test_release.sh
set -u
SRC="$(cd "$(dirname "$0")" && pwd)/release.sh"
ROOT="$(mktemp -d)"; trap 'rm -rf "$ROOT"' EXIT
PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "ok   - $1"; }
bad()  { FAIL=$((FAIL+1)); echo "FAIL - $1"; }
check(){ if eval "$2"; then ok "$1"; else bad "$1"; fi; }
G() { git -c user.email=t@t -c user.name=t "$@"; }

mkrepo() { # name -> $ROOT/name (clone) with bare origin, one commit on main
  git init -q --bare "$ROOT/$1.git"
  git init -q "$ROOT/$1"; ( cd "$ROOT/$1" && git config user.email t@t && git config user.name t && git checkout -q -b main && mkdir -p Formula scripts \
    && echo a > f && G add f && G commit -qm one && git remote add origin "$ROOT/$1.git" && git push -q origin main )
}
mkrepo runner; mkrepo tap; mkrepo backend
cat > "$ROOT/runner/Cargo.toml" <<'T'
[workspace]
members = []

[workspace.package]
version = "9.9.9"
T
( cd "$ROOT/runner" && G add Cargo.toml && G commit -qm cargo && G tag v9.9.9 && git push -q origin main v9.9.9 )
# Formula template with 2 shas
S1=$(printf 1 | shasum -a 256 | cut -d' ' -f1); S2=$(printf 2 | shasum -a 256 | cut -d' ' -f1)
cat > "$ROOT/formula.good" <<T
class Scenery < Formula
  version "9.9.9"
  on_macos do
    url "x"
    sha256 "$S1"
    url "y"
    sha256 "$S2"
  end
end
T
sed 's/9.9.9/9.9.8/' "$ROOT/formula.good" > "$ROOT/formula.old"
cp "$ROOT/formula.old" "$ROOT/tap/Formula/scenery.rb"
( cd "$ROOT/tap" && G add -A && G commit -qm formula && git push -q origin main )
echo "$S1  scenery-9.9.9-a.tar.gz" > "$ROOT/SUMS"; echo "$S2  scenery-9.9.9-b.tar.gz" >> "$ROOT/SUMS"

BIN="$ROOT/bin"; LOG="$ROOT/calls.log"; mkdir -p "$BIN"
cat > "$BIN/gh" <<'S'
#!/bin/bash
echo "GH $*" >> "$CALLS"
case "$1 $2" in
  "run list") echo "${FAKE_RUN:-completed success}" ;;
  "release view")
    case "$*" in
      *isDraft*) echo "${FAKE_DRAFT:-false}" ;;
      *) echo "${FAKE_ASSETS:-scenery-9.9.9-a.tar.gz SHA256SUMS}" | tr ' ' '\n' ;;
    esac ;;
  "release download")
    while [ $# -gt 0 ]; do [ "$1" = "--dir" ] && d="$2"; shift; done
    cp "$SUMS" "$d/SHA256SUMS" ;;
esac
exit 0
S
cat > "$BIN/sign" <<'S'
#!/bin/bash
echo "SIGN $* formula=$SCENERY_FORMULA" >> "$CALLS"
[ -n "${FAKE_SIGN_FAIL:-}" ] && exit 1
cp "$GOOD" "$SCENERY_FORMULA"   # what --apply-formula would do
exit 0
S
cat > "$BIN/stage" <<'S'
#!/bin/bash
echo "STAGE $*" >> "$CALLS"
exit 0
S
chmod +x "$BIN"/*
touch "$ROOT/fake.key"
export CALLS="$LOG" SUMS="$ROOT/SUMS" GOOD="$ROOT/formula.good" \
  REL_GH="$BIN/gh" REL_SIGN="$BIN/sign" REL_STAGE="$BIN/stage" REL_REPO=o/r \
  REL_RUNNER_DIR="$ROOT/runner" REL_TAP_DIR="$ROOT/tap" REL_BACKEND_DIR="$ROOT/backend" \
  REL_NOW="6 12" REL_POLL_SLEEP=0 REL_POLL_TRIES=2 SCENERY_MINISIGN_KEY="$ROOT/fake.key"

reset() { : > "$LOG"; unset FAKE_RUN FAKE_ASSETS FAKE_SIGN_FAIL FAKE_DRAFT
  ( cd "$ROOT/tap" && git checkout -q main && git reset -q --hard origin/main ); }
run() { "$SRC" "$@" >"$ROOT/out" 2>&1; echo $? >"$ROOT/rc"; }
rc() { cat "$ROOT/rc"; }
line() { grep -n "$1" "$LOG" | head -1 | cut -d: -f1; }
tap_head() { git -C "$ROOT/tap" log -1 --format=%s; }
origin_head() { git -C "$ROOT/tap.git" log -1 --format=%s main; }

# 1. args
reset; run;                       check "no tag rejected" '[ "$(rc)" = 2 ]'
reset; run 1.2.3;                 check "tag without v rejected" '[ "$(rc)" = 2 ]'
reset; run v9.9.9 --from bogus;   check "bad --from rejected" '[ "$(rc)" = 2 ]'
reset; run v9.9.9 --nope;         check "unknown option rejected" '[ "$(rc)" = 2 ]'

# 2. preflight failures (nothing external runs)
reset; run v9.9.10
check "tag missing on origin fails" '[ "$(rc)" = 1 ] && grep -q "does not exist on origin" "$ROOT/out" && [ ! -s "$LOG" ]'
( cd "$ROOT/runner" && G tag v9.9.8 && git push -q origin v9.9.8 )
reset; run v9.9.8
check "Cargo.toml version mismatch fails" '[ "$(rc)" = 1 ] && grep -q "workspace version is 9.9.9" "$ROOT/out" && [ ! -s "$LOG" ]'
reset; echo dirty > "$ROOT/tap/junk"; run v9.9.9
check "dirty tap fails" '[ "$(rc)" = 1 ] && grep -q "homebrew-tap: working tree is not clean" "$ROOT/out" && [ ! -s "$LOG" ]'
rm "$ROOT/tap/junk"
reset; echo dirty > "$ROOT/backend/junk"; run v9.9.9
check "dirty backend fails" '[ "$(rc)" = 1 ] && grep -q "scenery-backend: working tree is not clean" "$ROOT/out"'
rm "$ROOT/backend/junk"
reset; ( cd "$ROOT/backend" && git checkout -q -b other ); run v9.9.9
check "backend off main fails" '[ "$(rc)" = 1 ] && grep -q "expected main" "$ROOT/out"'
( cd "$ROOT/backend" && git checkout -q main && git branch -q -D other )
reset; ( cd "$ROOT/backend" && echo z >> f && G commit -qam ahead ); run v9.9.9
check "backend ahead of origin fails" '[ "$(rc)" = 1 ] && grep -q "not equal to origin/main" "$ROOT/out"'
( cd "$ROOT/backend" && git reset -q --hard origin/main )

# 3. hours guard (REL_NOW = Saturday noon is fine; Wed 10am is not)
reset; REL_NOW="3 10" run v9.9.9
check "weekday 10 ET refused before anything runs" '[ "$(rc)" = 1 ] && grep -q "9am-5pm ET" "$ROOT/out" && [ ! -s "$LOG" ]'
check "hours refusal hints a resume that skips nothing" 'grep -q -- "--from preflight" "$ROOT/out" && ! grep -q -- "--from formula" "$ROOT/out"'
reset; REL_NOW="3 17" run v9.9.9
check "5pm ET allowed" '[ "$(rc)" = 0 ]'
reset; REL_NOW="3 08" run v9.9.9
check "8am ET allowed (leading zero ok)" '[ "$(rc)" = 0 ]'
reset; REL_NOW="3 10" run v9.9.9 --force-hours
check "--force-hours overrides" '[ "$(rc)" = 0 ] && [ "$(origin_head)" = "scenery 9.9.9" ]'

# 4. happy path, unsigned: order and effects
reset; run v9.9.9
check "full run ok" '[ "$(rc)" = 0 ]'
check "step order: run list, release view, sign, download, stage" '[ "$(line "GH run list")" -lt "$(line "GH release view")" ] && [ "$(line "GH release view")" -lt "$(line "^SIGN")" ] && [ "$(line "^SIGN")" -lt "$(line "GH release download")" ] && [ "$(line "GH release download")" -lt "$(line "^STAGE")" ]'
check "sign called with tag, --apply-formula, tap formula" 'grep -q "^SIGN v9.9.9 --apply-formula formula=$ROOT/tap/Formula/scenery.rb" "$LOG"'
check "stage called with the tag" 'grep -qx "STAGE v9.9.9" "$LOG"'
check "tap committed as 'scenery 9.9.9' and pushed" '[ "$(tap_head)" = "scenery 9.9.9" ] && [ "$(origin_head)" = "scenery 9.9.9" ]'
check "summary + brew upgrade reminder" 'grep -q "summary" "$ROOT/out" && grep -q "brew upgrade scenery" "$ROOT/out"'
check "key path never printed" '! grep -q "fake.key" "$ROOT/out"'

# 5. already signed: skip sign
reset; run v9.9.9
reset; FAKE_ASSETS="SHA256SUMS SHA256SUMS.minisig" run v9.9.9
check "already signed: sign skipped, stage still runs" '[ "$(rc)" = 0 ] && ! grep -q "^SIGN" "$LOG" && grep -q "already signed" "$ROOT/out" && grep -q "^STAGE" "$LOG"'
check "already signed + formula already current: no new commit, still ok" '[ "$(tap_head)" = "scenery 9.9.9" ]'
reset; cp "$ROOT/formula.old" "$ROOT/tap/Formula/scenery.rb"; ( cd "$ROOT/tap" && G commit -qam rollback && git push -q origin main )
FAKE_ASSETS="SHA256SUMS SHA256SUMS.minisig" run v9.9.9
check "signed but formula stale: fails at formula, stage not run" '[ "$(rc)" = 1 ] && grep -q "ERROR \[formula\]" "$ROOT/out" && ! grep -q "^STAGE" "$LOG" && grep -q -- "--from formula" "$ROOT/out"'

reset; FAKE_DRAFT=true FAKE_ASSETS="SHA256SUMS SHA256SUMS.minisig" run v9.9.9
check "signed but still a draft: refused before formula, nothing pushed" '[ "$(rc)" = 1 ] && grep -q "still a draft" "$ROOT/out" && grep -q -- "--from sign" "$ROOT/out" && ! grep -q "^STAGE" "$LOG" && [ "$(origin_head)" = "rollback" ]'

# 6. workflow wait
reset; FAKE_RUN="completed failure" run v9.9.9
check "failed workflow fails clearly, nothing after" '[ "$(rc)" = 1 ] && grep -q "conclusion .failure." "$ROOT/out" && ! grep -q "^SIGN" "$LOG" && ! grep -q "^STAGE" "$LOG"'
reset; FAKE_RUN="in_progress " run v9.9.9
check "workflow never finishing times out" '[ "$(rc)" = 1 ] && grep -q "timed out" "$ROOT/out" && [ "$(grep -c "GH run list" "$LOG")" = 3 ]'

# 7. sign failure and --from
reset; FAKE_SIGN_FAIL=1 run v9.9.9
check "sign failure stops before formula/stage, hints --from sign" '[ "$(rc)" = 1 ] && grep -q -- "--from sign" "$ROOT/out" && ! grep -q "^STAGE" "$LOG" && [ "$(tap_head)" != "scenery 9.9.9" ]'
reset; run v9.9.9 --from stage
check "--from stage: only stage runs (no gh, no sign)" '[ "$(rc)" = 0 ] && [ "$(wc -l < "$LOG" | tr -d " ")" = 1 ] && grep -qx "STAGE v9.9.9" "$LOG"'
reset; run v9.9.9 --from sign; # formula already pushed by previous run, formula step no-ops
check "--from sign skips preflight and wait" '[ "$(rc)" = 0 ] && ! grep -q "GH run list" "$LOG" && grep -q "^SIGN" "$LOG"'
reset; echo dirty > "$ROOT/tap/junk"; run v9.9.9 --from wait; rm "$ROOT/tap/junk"
check "--from wait skips preflight (dirty tap ignored)" '[ "$(rc)" = 0 ]'

echo; echo "passed=$PASS failed=$FAIL"; [ "$FAIL" = 0 ]
