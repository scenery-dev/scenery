#!/usr/bin/env bash
# dev-push.sh: test an UNRELEASED runner build on a Linux dev host (spaceghost)
# without tagging, signing, or Homebrew.
#
# Usage: scripts/dev-push.sh [<host>] [--revert] [--status]
#   <host>     ssh target, default spaceghost@pop-os.local
#   (none)     build current working tree on the host, install, activate
#   --status   show the binary the daemon runs, dev build info, brew version
#   --revert   remove the dev override and go back to the brew binary
#
# BUILD PATH: remote build. spaceghost (x86_64, Ubuntu glibc 2.39) has rustup
# with the exact toolchain in rust-toolchain.toml (1.99.0), gcc, and rsync, so
# we rsync the working tree (no target/, .git) to ~/.cache/scenery-dev-build
# and run `cargo build --release --locked -p scenery-cli` there. That avoids
# cross-compiling from macOS (zig/glibc shims) and guarantees the binary links
# against the glibc it will run on. The remote target/ is kept between pushes
# so rebuilds are incremental.
#
# INSTALL: ~/.local/share/Scenery/dev-bin/scenery (+ BUILD_INFO with git sha,
# dirty flag, build time), plus a ~/.local/bin/scenery-dev symlink. Brew's
# `scenery` on PATH is never shadowed; type `scenery-dev` to use the dev CLI.
#
# ACTIVATE: a SECOND systemd user drop-in
#   ~/.config/systemd/user/sh.brew.scenery.service.d/zz-dev-binary.conf
# resetting ExecStart= to the dev binary. local.conf (SCENERY_REGISTRY_URL,
# UMask) is never read or written. The "zz" prefix sorts it after local.conf.
#
# WARNING: `brew services restart` regenerates the unit but KEEPS drop-ins, so
# the dev override persists until you run --revert.
#
# macOS (launchd via `brew services`), doc-only, no --local mode:
#   brew services stop scenery
#   cargo build --release
#   SCENERY_REGISTRY_URL=... target/release/scenery daemon run --foreground
#   # when done: Ctrl-C, then `brew services start scenery`
# shellcheck disable=SC2016  # remote snippets are intentionally single-quoted
set -euo pipefail

HOST="spaceghost@pop-os.local"
MODE=push
for a in "$@"; do
  case "$a" in
    --revert) MODE=revert ;;
    --status) MODE=status ;;
    -h|--help) sed -n 2,12p "$0"; exit 0 ;;
    -*) echo "unknown flag: $a" >&2; exit 2 ;;
    *) HOST="$a" ;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# Prelude for every remote command: non-interactive SSH has no brew, cargo, or
# user-bus env (see root CLAUDE.md).
read -r -d '' PRE <<'PRE_EOF' || true
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:/home/linuxbrew/.linuxbrew/bin:$PATH"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
UNIT=sh.brew.scenery.service
DEVBIN="$HOME/.local/share/Scenery/dev-bin/scenery"
DROPDIR="$HOME/.config/systemd/user/$UNIT.d"
DROP="$DROPDIR/zz-dev-binary.conf"
daemon_exe() {
  local pid; pid=$(systemctl --user show -p MainPID --value "$UNIT")
  if [ -z "$pid" ] || [ "$pid" = 0 ]; then echo "(not running)"; else readlink "/proc/$pid/exe" || echo "(unreadable)"; fi
}
PRE_EOF

rsh() { ssh -o BatchMode=yes "$HOST" "bash -s" <<<"$PRE
$1"; }

do_status() {
  rsh '
echo "daemon exe : $(daemon_exe)"
echo "override   : $([ -f "$DROP" ] && echo ACTIVE || echo none)"
echo "dev build  : $(cat "$DEVBIN.info" 2>/dev/null || echo none)"
[ -x "$DEVBIN" ] && echo "dev version: $("$DEVBIN" --version 2>&1)"
b=$(command -v scenery || true)
echo "brew       : ${b:-not on PATH} $([ -n "$b" ] && "$b" --version 2>&1)"
'
}

do_push() {
  local sha dirty now
  sha=$(git -C "$ROOT" rev-parse --short=12 HEAD)
  dirty=clean; [ -n "$(git -C "$ROOT" status --porcelain)" ] && dirty=dirty
  now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  echo ">> rsync tree to $HOST (sha $sha, $dirty)"
  ssh -o BatchMode=yes "$HOST" 'mkdir -p ~/.cache/scenery-dev-build'
  rsync -az --delete --exclude /target --exclude .git -e "ssh -o BatchMode=yes" \
    "$ROOT"/ "$HOST":.cache/scenery-dev-build/
  echo ">> remote cargo build --release (niced)"
  rsh 'cd ~/.cache/scenery-dev-build && nice -n 10 cargo build --release --locked -p scenery-cli'
  echo ">> install + activate"
  rsh "
set -e
mkdir -p \"\$(dirname \"\$DEVBIN\")\" \"\$DROPDIR\" \"\$HOME/.local/bin\"
install -m 755 ~/.cache/scenery-dev-build/target/release/scenery \"\$DEVBIN.new\"
mv -f \"\$DEVBIN.new\" \"\$DEVBIN\"
echo 'sha=$sha $dirty built=$now' > \"\$DEVBIN.info\"
ln -sfn \"\$DEVBIN\" \"\$HOME/.local/bin/scenery-dev\"
[ -f \"\$DROPDIR/local.conf\" ] || echo 'NOTE: local.conf not found in drop-in dir (registry/UMask overrides absent)'
printf '[Service]\nExecStart=\nExecStart=%s daemon run --foreground\n' \"\$DEVBIN\" > \"\$DROP\"
systemctl --user daemon-reload
systemctl --user restart \"\$UNIT\"
sleep 2
exe=\$(daemon_exe); want=\$(readlink -f \"\$DEVBIN\")
echo \"daemon exe: \$exe\"
echo \"version   : \$(\"\$DEVBIN\" --version)\"
[ \"\$exe\" = \"\$want\" ] || { echo 'FAIL: daemon is not running the dev binary' >&2; exit 1; }
echo 'OK: daemon runs dev build'
"
  cat <<NOTE

*** DEV OVERRIDE ACTIVE on $HOST ***
- CLI for this build: scenery-dev (brew's \`scenery\` on PATH is untouched; it is a different binary).
- \`brew services restart\` regenerates the unit but KEEPS drop-ins, so the dev
  daemon persists until you run: scripts/dev-push.sh $HOST --revert
NOTE
}

do_revert() {
  rsh '
set -e
rm -f "$DROP" "$HOME/.local/bin/scenery-dev"
systemctl --user daemon-reload
systemctl --user restart "$UNIT"
sleep 2
exe=$(daemon_exe)
echo "daemon exe: $exe"
case "$exe" in
  */dev-bin/*|"(not running)"|"(unreadable)") echo "FAIL: daemon not on brew binary" >&2; exit 1 ;;
esac
echo "version   : $("$exe" --version 2>&1 || true)"
echo "OK: back on brew binary (dev-bin/ left in place; local.conf untouched)"
'
}

case "$MODE" in
  push) do_push ;;
  revert) do_revert ;;
  status) do_status ;;
esac
