#!/usr/bin/env bash
# release.sh vX.Y.Z [--from <step>] [--force-hours]
#
# One owner-run command for everything after you push a release tag. Run it in
# your OWN terminal: the sign step prompts for the minisign key password.
#
# Steps (in order; --from <step> resumes at that step, earlier ones are skipped):
#   preflight  tag is on origin, workspace Cargo.toml version matches, the
#              homebrew-tap and scenery-backend repos are clean, on main and
#              equal to origin/main
#   wait       wait for the release workflow run for the tag to succeed
#   sign       run scripts/sign-release.sh --apply-formula (skipped when the
#              release already has a SHA256SUMS.minisig asset)
#   formula    verify the formula (version + sha256s match the published
#              SHA256SUMS), commit "scenery X.Y.Z" and push homebrew-tap.
#              Refuses weekdays 9am-5pm ET unless --force-hours.
#   stage      run scenery-backend/demo/host/stage-runner-artifacts.sh <tag>
#   summary    always printed at the end (includes the spaceghost reminder)
#
# Environment (all optional):
#   SCENERY_MINISIGN_KEY  minisign secret key path (default ~/.ssh/scenery-release.key); never printed
#   REL_RUNNER_DIR        runner repo (default: parent of this script's dir)
#   REL_TAP_DIR           homebrew-tap checkout (default: ../homebrew-tap next to the runner repo)
#   REL_BACKEND_DIR       scenery-backend checkout (default: ../scenery-backend)
#   REL_GH, REL_SIGN, REL_STAGE   override gh / sign-release.sh / stage-runner-artifacts.sh
#   REL_REPO              GitHub repo (default scenery-dev/scenery)
#   REL_NOW               override the clock as "<ISO weekday 1-7> <hour 00-23>" in ET (tests)
#   REL_POLL_SLEEP, REL_POLL_TRIES   workflow polling (default 20s x 360)
#
# Works with bash 3.2 (macOS default).
set -euo pipefail

usage() { echo "usage: $0 vX.Y.Z [--from preflight|wait|sign|formula|stage] [--force-hours]" >&2; exit 2; }

TAG=""; FROM="preflight"; FORCE_HOURS=0
while [ $# -gt 0 ]; do
  case "$1" in
    --from) [ $# -ge 2 ] || usage; FROM="$2"; shift 2 ;;
    --force-hours) FORCE_HOURS=1; shift ;;
    -h|--help) usage ;;
    -*) echo "unknown option: $1" >&2; usage ;;
    *) [ -z "$TAG" ] || usage; TAG="$1"; shift ;;
  esac
done
case "$TAG" in v[0-9]*.[0-9]*.[0-9]*) ;; *) usage ;; esac
VERSION="${TAG#v}"
case "$FROM" in preflight|wait|sign|formula|stage) ;; *) echo "unknown step: $FROM" >&2; usage ;; esac

HERE="$(cd "$(dirname "$0")" && pwd)"
RUNNER="${REL_RUNNER_DIR:-$(cd "$HERE/.." && pwd)}"
TAP="${REL_TAP_DIR:-$RUNNER/../homebrew-tap}"
BACKEND="${REL_BACKEND_DIR:-$RUNNER/../scenery-backend}"
GH="${REL_GH:-gh}"
SIGN="${REL_SIGN:-$HERE/sign-release.sh}"
STAGE="${REL_STAGE:-$BACKEND/demo/host/stage-runner-artifacts.sh}"
REPO="${REL_REPO:-scenery-dev/scenery}"
FORMULA="$TAP/Formula/scenery.rb"
POLL_SLEEP="${REL_POLL_SLEEP:-20}"
POLL_TRIES="${REL_POLL_TRIES:-360}"

CURRENT="preflight"
SUMMARY=""
note() { SUMMARY="${SUMMARY}  $1
"; }
say() { echo "==> $*"; }
die() { echo "ERROR [$CURRENT]: $*" >&2; echo "Fix the problem, then resume with: $0 $TAG --from $CURRENT" >&2; exit 1; }

step_index() { case "$1" in preflight) echo 1;; wait) echo 2;; sign) echo 3;; formula) echo 4;; stage) echo 5;; esac; }
should_run() { [ "$(step_index "$1")" -ge "$(step_index "$FROM")" ]; }

in_work_hours() {
  local now dow hour
  now="${REL_NOW:-$(TZ=America/New_York date '+%u %H')}"
  dow="${now%% *}"; hour="${now##* }"; hour="$((10#$hour))"
  [ "$dow" -le 5 ] && [ "$hour" -ge 9 ] && [ "$hour" -lt 17 ]
}
hours_guard() {
  if in_work_hours && [ "$FORCE_HOURS" = 0 ]; then
    die "weekday 9am-5pm ET: no commits during this window (project rule). Re-run later, or pass --force-hours."
  fi
}

repo_ready() { # <dir> <label>
  local d="$1" l="$2" br
  [ -d "$d/.git" ] || die "$l: $d is not a git checkout"
  br="$(git -C "$d" branch --show-current)"
  [ "$br" = "main" ] || die "$l: on branch '$br', expected main"
  [ -z "$(git -C "$d" status --porcelain)" ] || die "$l: working tree is not clean"
  git -C "$d" fetch -q origin main || die "$l: git fetch failed"
  [ "$(git -C "$d" rev-parse HEAD)" = "$(git -C "$d" rev-parse origin/main)" ] \
    || die "$l: main is not equal to origin/main (pull or push first)"
}

# Fail fast on the hours rule if we will reach the commit step.
# CURRENT stays at the first step to run, so the resume hint does not skip steps.
if should_run formula; then CURRENT="$FROM"; hours_guard; fi

if should_run preflight; then
  CURRENT="preflight"; say "Preflight"
  git -C "$RUNNER" ls-remote --exit-code --tags origin "refs/tags/$TAG" >/dev/null 2>&1 \
    || die "tag $TAG does not exist on origin (runner repo)"
  cv="$(awk '/^\[workspace.package\]/ {w=1; next} /^\[/ {w=0} w && /^version *=/ {gsub(/"/,"",$3); print $3; exit}' "$RUNNER/Cargo.toml")"
  [ -n "$cv" ] || die "could not read workspace version from $RUNNER/Cargo.toml"
  [ "$cv" = "$VERSION" ] || die "Cargo.toml workspace version is $cv, tag is $TAG"
  repo_ready "$TAP" "homebrew-tap"
  repo_ready "$BACKEND" "scenery-backend"
  note "preflight: ok (tag on origin, Cargo.toml $cv, tap + backend clean and on origin/main)"
else note "preflight: skipped (--from $FROM)"; fi

if should_run wait; then
  CURRENT="wait"; say "Waiting for the release workflow run for $TAG"
  i=0
  while :; do
    out="$("$GH" run list -R "$REPO" --workflow release.yml --branch "$TAG" --limit 1 \
      --json status,conclusion -q '.[0] | "\(.status) \(.conclusion)"' 2>/dev/null || true)"
    st="${out%% *}"; co="${out#* }"
    if [ "$st" = "completed" ]; then
      [ "$co" = "success" ] || die "release workflow for $TAG finished with conclusion '$co' ($GH run list -R $REPO --branch $TAG)"
      break
    fi
    i=$((i+1))
    [ "$i" -le "$POLL_TRIES" ] || die "timed out waiting for the release workflow for $TAG (last status: '${st:-none}')"
    echo "    status: ${st:-no run yet}; waiting ${POLL_SLEEP}s ($i/$POLL_TRIES)"
    sleep "$POLL_SLEEP"
  done
  note "wait: workflow succeeded"
else note "wait: skipped"; fi

if should_run sign; then
  CURRENT="sign"
  assets="$("$GH" release view "$TAG" -R "$REPO" --json assets -q '.assets[].name')" || die "cannot read release $TAG"
  if printf '%s\n' "$assets" | grep -qx 'SHA256SUMS.minisig'; then
    # sign-release.sh uploads the .minisig before publishing: a failure between the
    # two leaves a signed DRAFT. Never ship a formula pointing at draft assets.
    draft="$("$GH" release view "$TAG" -R "$REPO" --json isDraft -q .isDraft)" || die "cannot read release $TAG"
    [ "$draft" = "false" ] || die "release $TAG is signed but still a draft; publish it (gh release edit $TAG -R $REPO --draft=false), then resume"
    say "Release $TAG already signed (SHA256SUMS.minisig present); skipping sign"
    note "sign: skipped (already signed)"
  else
    export SCENERY_MINISIGN_KEY="${SCENERY_MINISIGN_KEY:-$HOME/.ssh/scenery-release.key}"
    [ -f "$SCENERY_MINISIGN_KEY" ] || die "minisign key not found; set SCENERY_MINISIGN_KEY"
    say "Signing $TAG (you will be prompted for the minisign password)"
    # Inherits this terminal's stdin/tty. SCENERY_FORMULA points at our tap.
    SCENERY_FORMULA="$FORMULA" "$SIGN" "$TAG" --apply-formula || die "sign-release.sh failed"
    note "sign: signed and published, formula updated"
  fi
else note "sign: skipped"; fi

if should_run formula; then
  CURRENT="formula"; hours_guard
  say "Verifying formula against the published SHA256SUMS"
  [ -f "$FORMULA" ] || die "formula not found at $FORMULA"
  tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
  "$GH" release download "$TAG" -R "$REPO" --pattern SHA256SUMS --dir "$tmp" --clobber || die "cannot download SHA256SUMS"
  [ -s "$tmp/SHA256SUMS" ] || die "SHA256SUMS missing or empty"
  grep -q "^  version \"$VERSION\"\$" "$FORMULA" || die "formula version is not $VERSION"
  while read -r sum _; do
    [ -n "$sum" ] || continue
    grep -q "sha256 \"$sum\"" "$FORMULA" || die "formula is missing sha256 $sum from SHA256SUMS (run sign-release.sh --apply-formula logic or edit the formula)"
  done < "$tmp/SHA256SUMS"
  # Every sha256 in the formula must come from this release (no stale lines).
  for sum in $(grep -o 'sha256 "[0-9a-f]*"' "$FORMULA" | sed 's/sha256 "//; s/"//'); do
    grep -q "^$sum " "$tmp/SHA256SUMS" || die "formula has stale sha256 $sum not in SHA256SUMS for $TAG"
  done
  repo_ready_tap_branch="$(git -C "$TAP" branch --show-current)"
  [ "$repo_ready_tap_branch" = "main" ] || die "homebrew-tap is on '$repo_ready_tap_branch', expected main"
  if [ -n "$(git -C "$TAP" status --porcelain -- Formula/scenery.rb)" ]; then
    git -C "$TAP" add Formula/scenery.rb
    git -C "$TAP" commit -q -m "scenery $VERSION" -- Formula/scenery.rb || die "git commit failed"
    say "Committed: scenery $VERSION"
  else
    say "Formula already committed; nothing to commit"
  fi
  git -C "$TAP" push -q origin main || die "git push failed (homebrew-tap)"
  note "formula: verified, committed and pushed homebrew-tap (scenery $VERSION)"
else note "formula: skipped"; fi

if should_run stage; then
  CURRENT="stage"; say "Staging $TAG on the demo box"
  "$STAGE" "$TAG" || die "stage-runner-artifacts.sh failed"
  note "stage: demo box serving $TAG"
else note "stage: skipped"; fi

echo
echo "==> Release $TAG summary"
printf '%s' "$SUMMARY"
echo
echo "Next: on spaceghost run"
echo "  ssh spaceghost@pop-os.local 'eval \"\$(/home/linuxbrew/.linuxbrew/bin/brew shellenv)\"; brew upgrade scenery && systemctl --user restart sh.brew.scenery.service'"
echo "Also: backend must already be live on prod for $VERSION; consider the demo-sync agent if user-visible."
