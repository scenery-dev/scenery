#!/usr/bin/env bash
#
# VectorHawk standalone uninstaller — restores your system, no VectorHawk binary
# required. Use this when `vectorhawk uninstall` won't run (binary missing or
# broken) and you just want VectorHawk fully off the machine.
#
# It is path-based and reads the on-disk restore journal directly, so it needs
# no running daemon. It mirrors the steps of the built-in `vectorhawk uninstall`
# and prints the same REMOVED / KEPT / RESTORED report.
#
# Mirror of the tier-1 command — keep in sync with crates/vectorhawkd-cli/src/uninstall.rs.
# Ship a copy in homebrew-tap so `curl … | bash` works even after `brew uninstall`.
#
# Usage:
#   ./uninstall.sh            # interactive
#   ./uninstall.sh --yes      # no prompt
#   ./uninstall.sh --purge    # also delete the data dir + logs
set -euo pipefail

YES=0; PURGE=0
for a in "$@"; do
  case "$a" in
    --yes|-y) YES=1 ;;
    --purge)  PURGE=1 ;;
    *) echo "unknown flag: $a" >&2; exit 2 ;;
  esac
done

# ── Platform paths ────────────────────────────────────────────────────────────
if [[ "$(uname)" == "Darwin" ]]; then
  DATA="$HOME/Library/Application Support/VectorHawk"
  LOGS="$HOME/Library/Logs/VectorHawk"
  PLIST="$HOME/Library/LaunchAgents/com.vectorhawk.agent.plist"
else
  DATA="${XDG_DATA_HOME:-$HOME/.local/share}/vectorhawk"
  LOGS="$DATA/logs"
  UNIT="$HOME/.config/systemd/user/vectorhawk-agent.service"
  AUTOSTART="$HOME/.config/autostart/vectorhawk.desktop"
fi
JOURNAL="$DATA/restore-journal.json"   # written by the runner (see journal card)
STAMP="$(date +%Y%m%dT%H%M%S 2>/dev/null || echo latest)"
REPORT="$HOME/VectorHawk-uninstall-$STAMP.md"

# Client configs VectorHawk may have written into.
CLIENT_CONFIGS=(
  "$HOME/.claude.json"
  "$HOME/.cursor/mcp.json"
  "$HOME/.codeium/windsurf/mcp_config.json"
  "$HOME/.gemini/settings.json"
  "$HOME/Library/Application Support/Claude/claude_desktop_config.json"
  "$HOME/Library/Application Support/Code/User/settings.json"
  "$HOME/.config/Claude/claude_desktop_config.json"
  "$HOME/.config/Code/User/settings.json"
)

have() { command -v "$1" >/dev/null 2>&1; }

if [[ "$YES" -ne 1 ]]; then
  echo "This will remove VectorHawk and restore your AI-client configs."
  read -r -p "Proceed? [y/N] " ans
  [[ "$ans" =~ ^[Yy] ]] || { echo "Aborted."; exit 0; }
fi

REMOVED=(); RESTORED=(); WARN=()

# ── 1. Stop + remove the background service ───────────────────────────────────
if [[ "$(uname)" == "Darwin" ]]; then
  launchctl bootout "gui/$(id -u)/com.vectorhawk.agent" 2>/dev/null || true
  [[ -f "$PLIST" ]] && rm -f "$PLIST"
else
  systemctl --user disable --now vectorhawk-agent.service 2>/dev/null || true
  [[ -f "${UNIT:-}" ]] && rm -f "$UNIT" && systemctl --user daemon-reload 2>/dev/null || true
  [[ -f "${AUTOSTART:-}" ]] && rm -f "$AUTOSTART"
fi

# ── 2. Restore native takeovers + adopted originals from the journal ──────────
# Journal entries: {op, target_path, backup_path, source}. We restore anything
# the user brought (native/adopted); brokered/managed items are removed, not restored.
if [[ -f "$JOURNAL" ]] && have jq; then
  while IFS=$'\t' read -r src backup target; do
    [[ -z "$target" ]] && continue
    if [[ "$src" == "native" || "$src" == "adopted" ]] && [[ -e "$backup" ]]; then
      rm -rf "$target"; mkdir -p "$(dirname "$target")"; cp -R "$backup" "$target" \
        && RESTORED+=("$target") || WARN+=("could not restore $target")
    fi
  done < <(jq -r '.[] | [.source, .backup_path, .target_path] | @tsv' "$JOURNAL" 2>/dev/null)
else
  [[ -f "$JOURNAL" ]] || WARN+=("no restore journal found — restore limited to F1 backups")
  have jq || WARN+=("jq not installed — cannot replay restore journal")
fi

# ── 3. Strip VectorHawk entries from every client config ──────────────────────
# Removes the "vectorhawk" shim key AND any brokered per-slug keys the runner
# added. Brokered slugs are read from the journal; without jq we drop only the
# shim key. User-owned servers are left untouched (reported as KEPT).
BROKERED_SLUGS=()
if [[ -f "$JOURNAL" ]] && have jq; then
  while read -r slug; do [[ -n "$slug" ]] && BROKERED_SLUGS+=("$slug"); done \
    < <(jq -r '.[] | select(.source=="brokered") | .slug' "$JOURNAL" 2>/dev/null)
fi
for cfg in "${CLIENT_CONFIGS[@]}"; do
  [[ -f "$cfg" ]] || continue
  if have jq; then
    key="mcpServers"; [[ "$cfg" == *"Code/User/settings.json" ]] && key="mcp.servers"
    filter='if has("'$key'") then .["'$key'"] |= del(.vectorhawk) else . end'
    for slug in "${BROKERED_SLUGS[@]}"; do
      filter="$filter"' | if has("'$key'") then .["'$key'"] |= del(."'"$slug"'") else . end'
      REMOVED+=("$slug (from $(basename "$cfg"))")
    done
    tmp="$(mktemp)"
    if jq "$filter" "$cfg" > "$tmp" 2>/dev/null; then mv "$tmp" "$cfg"; else rm -f "$tmp"; WARN+=("could not edit $cfg"); fi
  else
    WARN+=("jq not installed — leaving $cfg untouched; remove the \"vectorhawk\" entry by hand")
  fi
done

# ── 4. Remove managed skills/plugins VectorHawk pushed into ~/.claude ─────────
for base in "$HOME/.claude/skills" "$HOME/.claude/plugins"; do
  [[ -d "$base" ]] || continue
  while IFS= read -r marker; do
    d="$(dirname "$marker")"; rm -rf "$d" && REMOVED+=("managed: $(basename "$d")")
  done < <(find "$base" -maxdepth 2 -name '.vectorhawk-managed.json' 2>/dev/null)
done

# ── 5. Optional hard purge ────────────────────────────────────────────────────
rm -f "$HOME/.claude.json.lock"
if [[ "$PURGE" -eq 1 ]]; then
  rm -rf "$DATA" "$LOGS"
  # keychain token (macOS); best-effort
  have security && security delete-generic-password -s "com.vectorhawk.agent" >/dev/null 2>&1 || true
fi

# ── 6. Write the restore report ───────────────────────────────────────────────
{
  echo "# VectorHawk uninstall — restore report"
  echo
  echo "## Removed completely"
  echo "_Re-add these on your own configuration; credentials were brokered and never stored locally._"
  echo
  if [[ ${#REMOVED[@]} -eq 0 ]]; then echo "- (none)"; else printf -- '- %s\n' "${REMOVED[@]}"; fi
  echo
  echo "## Restored"
  if [[ ${#RESTORED[@]} -eq 0 ]]; then echo "- (none)"; else printf -- '- %s\n' "${RESTORED[@]}"; fi
  echo
  if [[ ${#WARN[@]} -gt 0 ]]; then echo "## Warnings — check by hand"; echo; printf -- '- %s\n' "${WARN[@]}"; fi
} > "$REPORT"

echo
echo "Done. VectorHawk removed. Restore report: $REPORT"
echo "Restart your AI client(s) to apply the change."
