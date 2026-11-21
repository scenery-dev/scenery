#!/bin/bash
# Scenery pkg uninstaller. Run as root: sudo "/Library/Application Support/Scenery/uninstall.sh"
# Restores each logged-in user's client configs locally (no server calls),
# then removes the agent, binary, our symlinks and the install root.
set -uo pipefail

PKG_ROOT="/Library/Application Support/Scenery"
PKG_BIN="$PKG_ROOT/bin/scenery"
PLIST="/Library/LaunchAgents/dev.scenery.agent.plist"
LABEL="dev.scenery.agent"

# Prints the uid of each user with a loaded gui/<uid> domain. Never uid 0 or
# the Setup Assistant user. Uses absolute paths: Installer runs us as root
# with a minimal PATH.
gui_uids() {
  local cand u uid skip seen=" "
  skip="$(/usr/bin/id -u _mbsetupuser 2>/dev/null || true)"
  cand="$(/usr/bin/stat -f %u /dev/console 2>/dev/null || true)"
  for u in $(/usr/bin/who 2>/dev/null | /usr/bin/awk '{print $1}' | /usr/bin/sort -u); do
    cand="$cand $(/usr/bin/id -u "$u" 2>/dev/null || true)"
  done
  for uid in $cand; do
    [[ "$uid" =~ ^[0-9]+$ ]] || continue
    [[ "$uid" == "0" || "$uid" == "$skip" ]] && continue
    [[ "$seen" == *" $uid "* ]] && continue
    seen="$seen$uid "
    if /bin/launchctl print "gui/$uid" >/dev/null 2>&1; then
      echo "$uid"
    fi
  done
}

if [[ "$(/usr/bin/id -u)" != "0" ]]; then
  echo "scenery uninstall: must run as root (use sudo)" >&2
  exit 1
fi

for uid in $(gui_uids); do
  user="$(/usr/bin/id -nu "$uid" 2>/dev/null || true)"
  echo "scenery uninstall: stopping agent for uid $uid (${user:-unknown})"
  /bin/launchctl bootout "gui/$uid/$LABEL" >/dev/null 2>&1 || true
  if [[ -n "$user" && -x "$PKG_BIN" ]]; then
    home="$(/usr/bin/dscl . -read "/Users/$user" NFSHomeDirectory 2>/dev/null | /usr/bin/awk '{ $1=""; sub(/^ /,""); print }')"
    if [[ -z "$home" ]]; then
      echo "scenery uninstall: no home directory for $user, skipping local cleanup" >&2
      continue
    fi
    /bin/launchctl asuser "$uid" /usr/bin/sudo -H -u "$user" /usr/bin/env HOME="$home" "$PKG_BIN" uninstall --local-only --yes \
      || echo "scenery uninstall: local cleanup failed for $user, continuing" >&2
  fi
done

# The stand-down (`launchctl disable`) persists per user, including for users
# who are not logged in. Re-enable the per-user agents for every local user
# uid, plus every GUI uid found above. `enable` needs no session; errors ignored.
local_uids() {
  /usr/bin/dscl . -list /Users UniqueID 2>/dev/null \
    | /usr/bin/awk '$2 >= 501 { print $2 }'
}
all_uids="$( { local_uids; gui_uids; } | /usr/bin/sort -un)"
for uid in $all_uids; do
  for agent in com.scenery.agent sh.brew.scenery homebrew.mxcl.scenery; do
    /bin/launchctl enable "gui/$uid/$agent" >/dev/null 2>&1 || true
  done
done

/bin/rm -f "$PLIST"
for link in /usr/local/bin/scenery /usr/local/bin/scn; do
  if [[ -L "$link" && "$(/usr/bin/readlink "$link")" == "$PKG_BIN" ]]; then
    /bin/rm -f "$link"
  fi
done
/bin/rm -rf "$PKG_ROOT"
for pkgid in dev.scenery.runner dev.scenery.runner.devregistry dev.scenery.runner.prodregistry; do
  /usr/sbin/pkgutil --forget "$pkgid" >/dev/null 2>&1 || true
done
echo "scenery uninstall: done"
exit 0
