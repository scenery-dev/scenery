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
# JSON handling works with either `jq` or `python3` (config stripping must
# never be skipped just because jq is missing — see edit_json_del_keys below).
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

have() { command -v "$1" >/dev/null 2>&1; }

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
STAMP="${VECTORHAWK_UNINSTALL_STAMP:-$(date +%Y%m%dT%H%M%S 2>/dev/null || echo latest)}"
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

mcp_key_for_config() {
  case "$1" in
    *"Code/User/settings.json") echo "mcp.servers" ;;
    *) echo "mcpServers" ;;
  esac
}

# ── Portable JSON helpers ──────────────────────────────────────────────────────
# One logical "edit/query JSON" surface with two interchangeable backends: jq
# (preferred, faster) and python3 (fallback — present on macOS and virtually
# every Linux distro). Config stripping must ALWAYS run, even with no jq.

# edit_json_del_keys <file> <top-level-key> <key>...
# Deletes each <key> from the object at .[top-level-key] in <file>, writing the
# result back if anything changed. Prints the keys that were actually present
# (and thus removed), one per line. Exit 0 = file processed (0+ keys removed);
# exit 1 = file missing/unreadable/not an object, nothing changed.
edit_json_del_keys() {
  local file="$1" topkey="$2"; shift 2
  [[ -f "$file" ]] || return 1
  [[ $# -eq 0 ]] && return 0
  if have jq; then
    _edit_json_del_keys_jq "$file" "$topkey" "$@"
  else
    _edit_json_del_keys_py "$file" "$topkey" "$@"
  fi
}

_edit_json_del_keys_jq() {
  local file="$1" topkey="$2"; shift 2
  local keys_json present_json tmp
  keys_json="$(printf '%s\n' "$@" | jq -R . | jq -s .)" || return 1
  present_json="$(jq --arg tk "$topkey" --argjson ks "$keys_json" '
    ( (.[$tk] // {}) ) as $o
    | if ($o | type) == "object"
      then [ $ks[] as $k | select($o | has($k)) | $k ]
      else [] end
  ' "$file" 2>/dev/null)" || return 1
  [[ -z "$present_json" ]] && return 1
  if [[ "$present_json" != "[]" ]]; then
    tmp="$(mktemp)"
    if jq --arg tk "$topkey" --argjson ks "$present_json" '
      if has($tk) then .[$tk] |= (reduce $ks[] as $k (.; del(.[$k]))) else . end
    ' "$file" > "$tmp" 2>/dev/null; then
      mv "$tmp" "$file"
    else
      rm -f "$tmp"; return 1
    fi
  fi
  printf '%s\n' "$present_json" | jq -r '.[]?'
  return 0
}

_edit_json_del_keys_py() {
  local file="$1" topkey="$2"; shift 2
  python3 - "$file" "$topkey" "$@" <<'PYEOF'
import json, sys
file, topkey, *keys = sys.argv[1:]
try:
    with open(file, encoding="utf-8") as fh:
        data = json.load(fh)
except Exception:
    sys.exit(1)
if not isinstance(data, dict):
    sys.exit(1)
obj = data.get(topkey)
present = []
if isinstance(obj, dict):
    present = [k for k in keys if k in obj]
    if present:
        for k in present:
            del obj[k]
        with open(file, "w", encoding="utf-8") as fh:
            json.dump(data, fh, indent=2)
            fh.write("\n")
for k in present:
    print(k)
sys.exit(0)
PYEOF
}

# json_list_keys <file> <top-level-key> — one key per line, or nothing if the
# file/key doesn't exist. Read-only.
json_list_keys() {
  local file="$1" topkey="$2"
  [[ -f "$file" ]] || return 0
  if have jq; then
    jq -r --arg tk "$topkey" '(.[$tk] // {}) | if type=="object" then keys[] else empty end' "$file" 2>/dev/null
  else
    python3 - "$file" "$topkey" <<'PYEOF'
import json, sys
file, topkey = sys.argv[1], sys.argv[2]
try:
    with open(file, encoding="utf-8") as fh:
        data = json.load(fh)
except Exception:
    sys.exit(0)
if not isinstance(data, dict):
    sys.exit(0)
obj = data.get(topkey)
if isinstance(obj, dict):
    for k in obj.keys():
        print(k)
PYEOF
  fi
}

# ── Restore-journal readers (portable, read-only) ─────────────────────────────
have_json_tool() { have jq || have python3; }

journal_usable() { [[ -f "$JOURNAL" ]] && have_json_tool; }

# "source<TAB>backup_path<TAB>target_path" for source in {native,adopted}.
journal_restore_rows() {
  journal_usable || return 0
  if have jq; then
    jq -r '.[] | select(.source=="native" or .source=="adopted") | [.source, (.backup_path // ""), (.target_path // "")] | @tsv' "$JOURNAL" 2>/dev/null
  else
    python3 - "$JOURNAL" <<'PYEOF'
import json, sys
try:
    with open(sys.argv[1], encoding="utf-8") as f:
        data = json.load(f)
except Exception:
    sys.exit(0)
if not isinstance(data, list):
    sys.exit(0)
for e in data:
    if isinstance(e, dict) and e.get("source") in ("native", "adopted"):
        print("\t".join([
            str(e.get("source", "")),
            str(e.get("backup_path", "") or ""),
            str(e.get("target_path", "") or ""),
        ]))
PYEOF
  fi
}

# One config key per line for entries with source=="brokered" — the actual key
# to strip from client configs (detail.server_key if present, else slug).
journal_brokered_keys() {
  journal_usable || return 0
  if have jq; then
    jq -r '.[] | select(.source=="brokered") | (.detail.server_key // .slug // empty)' "$JOURNAL" 2>/dev/null
  else
    python3 - "$JOURNAL" <<'PYEOF'
import json, sys
try:
    with open(sys.argv[1], encoding="utf-8") as f:
        data = json.load(f)
except Exception:
    sys.exit(0)
if not isinstance(data, list):
    sys.exit(0)
for e in data:
    if not isinstance(e, dict) or e.get("source") != "brokered":
        continue
    detail = e.get("detail") or {}
    key = detail.get("server_key") or e.get("slug")
    if key:
        print(key)
PYEOF
  fi
}

# Every key (slug AND detail.server_key, any source) VectorHawk ever wrote into
# a client config — used to tell VH-authored entries apart from the user's own
# when computing the KEPT section.
journal_all_keys() {
  journal_usable || return 0
  if have jq; then
    jq -r '.[] | (.slug // empty, .detail.server_key // empty)' "$JOURNAL" 2>/dev/null | sed '/^$/d'
  else
    python3 - "$JOURNAL" <<'PYEOF'
import json, sys
try:
    with open(sys.argv[1], encoding="utf-8") as f:
        data = json.load(f)
except Exception:
    sys.exit(0)
if not isinstance(data, list):
    sys.exit(0)
for e in data:
    if not isinstance(e, dict):
        continue
    if e.get("slug"):
        print(e["slug"])
    detail = e.get("detail") or {}
    if detail.get("server_key"):
        print(detail["server_key"])
PYEOF
  fi
}

if [[ "$YES" -ne 1 ]]; then
  echo "This will remove VectorHawk and restore your AI-client configs."
  read -r -p "Proceed? [y/N] " ans
  [[ "$ans" =~ ^[Yy] ]] || { echo "Aborted."; exit 0; }
fi

REMOVED=(); KEPT=(); RESTORED=(); WARN=()

if [[ -f "$JOURNAL" ]] && ! have_json_tool; then
  WARN+=("neither jq nor python3 is available — cannot read the restore journal or edit client configs; remove VectorHawk entries by hand")
elif [[ ! -f "$JOURNAL" ]]; then
  WARN+=("no restore journal found — restore limited to what's already backed up on disk; only the literal \"vectorhawk\" key is treated as VectorHawk-authored")
fi

# ── 1. Plan (read-only): compute KEPT before anything is mutated ──────────────
# Any config key that's VectorHawk-authored (per the journal, any source) or is
# the shim's own "vectorhawk" key is NOT a user tool; everything else present
# is the user's own and gets reported as kept, untouched.
VH_KEYS=("vectorhawk")
if have_json_tool; then
  while IFS= read -r k; do [[ -n "$k" ]] && VH_KEYS+=("$k"); done < <(journal_all_keys)
fi
is_vh_key() {
  local needle="$1" k
  for k in "${VH_KEYS[@]}"; do [[ "$k" == "$needle" ]] && return 0; done
  return 1
}
for cfg in "${CLIENT_CONFIGS[@]}"; do
  [[ -f "$cfg" ]] || continue
  key="$(mcp_key_for_config "$cfg")"
  while IFS= read -r present; do
    [[ -z "$present" ]] && continue
    is_vh_key "$present" || KEPT+=("$present|$(basename "$cfg")")
  done < <(json_list_keys "$cfg" "$key")
done

# ── 2. Stop + remove the background service ────────────────────────────────────
if [[ "$(uname)" == "Darwin" ]]; then
  launchctl bootout "gui/$(id -u)/com.vectorhawk.agent" 2>/dev/null || true
  [[ -f "$PLIST" ]] && rm -f "$PLIST"
else
  systemctl --user disable --now vectorhawk-agent.service 2>/dev/null || true
  [[ -f "${UNIT:-}" ]] && rm -f "$UNIT" && systemctl --user daemon-reload 2>/dev/null || true
  [[ -f "${AUTOSTART:-}" ]] && rm -f "$AUTOSTART"
fi

# ── 3. Restore native takeovers + adopted originals from the journal ──────────
# Journal entries: {op, source, target_path, backup_path, ...}. We restore
# anything the user brought (native/adopted); brokered/managed items are
# removed in step 4, not restored.
if have_json_tool; then
  while IFS=$'\t' read -r src backup target; do
    [[ -z "$target" ]] && continue
    if [[ -n "$backup" && -e "$backup" ]]; then
      rm -rf "$target"; mkdir -p "$(dirname "$target")"; cp -R "$backup" "$target" \
        && RESTORED+=("$target") || WARN+=("could not restore $target")
    else
      WARN+=("backup missing for $target (source=$src) — could not restore")
    fi
  done < <(journal_restore_rows)
fi

# ── 4. Strip VectorHawk entries from every client config ──────────────────────
# Removes the "vectorhawk" shim key AND any brokered per-slug keys the runner
# added. Uses jq if available, otherwise a python3 fallback — config stripping
# must always happen, never silently skipped.
BROKERED_KEYS=()
if have_json_tool; then
  while IFS= read -r k; do [[ -n "$k" ]] && BROKERED_KEYS+=("$k"); done < <(journal_brokered_keys)
fi
for cfg in "${CLIENT_CONFIGS[@]}"; do
  [[ -f "$cfg" ]] || continue
  key="$(mcp_key_for_config "$cfg")"
  if ! have_json_tool; then
    WARN+=("no jq or python3 — leaving $cfg untouched; remove the \"vectorhawk\" entry by hand")
    continue
  fi
  removed_here="$(edit_json_del_keys "$cfg" "$key" "vectorhawk" "${BROKERED_KEYS[@]+"${BROKERED_KEYS[@]}"}")" || {
    WARN+=("could not parse/edit $cfg — remove the \"vectorhawk\" entry by hand")
    continue
  }
  while IFS= read -r r; do
    [[ -z "$r" ]] && continue
    if [[ "$r" == "vectorhawk" ]]; then
      REMOVED+=("vectorhawk shim (from $(basename "$cfg"))")
    else
      REMOVED+=("$r (from $(basename "$cfg"))")
    fi
  done <<< "$removed_here"
done

# ── 5. Remove managed skills/plugins VectorHawk pushed into ~/.claude ─────────
for base in "$HOME/.claude/skills" "$HOME/.claude/plugins"; do
  [[ -d "$base" ]] || continue
  while IFS= read -r marker; do
    d="$(dirname "$marker")"; rm -rf "$d" && REMOVED+=("managed: $(basename "$d")")
  done < <(find "$base" -maxdepth 2 -name '.vectorhawk-managed.json' 2>/dev/null)
done

# ── 6. Optional hard purge ────────────────────────────────────────────────────
rm -f "$HOME/.claude.json.lock"
if [[ "$PURGE" -eq 1 ]]; then
  rm -rf "$DATA" "$LOGS"
  # keychain token (macOS); best-effort
  have security && security delete-generic-password -s "com.vectorhawk.agent" >/dev/null 2>&1 || true
fi

# ── 7. Write the restore report ───────────────────────────────────────────────
{
  echo "# VectorHawk uninstall — restore report"
  echo
  echo "## Removed completely"
  echo "_Re-add these on your own configuration; credentials were brokered and never stored locally._"
  echo
  if [[ ${#REMOVED[@]} -eq 0 ]]; then echo "- (none)"; else printf -- '- %s\n' "${REMOVED[@]}"; fi
  echo
  echo "## Kept (no longer managed by VectorHawk)"
  echo "_Your own tools — left exactly as they were, just unregistered._"
  echo
  if [[ ${#KEPT[@]} -eq 0 ]]; then
    echo "- (none)"
  else
    for entry in "${KEPT[@]}"; do
      name="${entry%%|*}"; loc="${entry##*|}"
      echo "- **$name** ($loc) — your own server, left in place, no longer audited by VectorHawk"
    done
  fi
  echo
  echo "## Restored"
  if [[ ${#RESTORED[@]} -eq 0 ]]; then echo "- (none)"; else printf -- '- %s\n' "${RESTORED[@]}"; fi
  echo
  if [[ ${#WARN[@]} -gt 0 ]]; then echo "## Warnings — please check by hand"; echo; printf -- '- %s\n' "${WARN[@]}"; fi
} > "$REPORT"

echo
echo "Done. VectorHawk removed."
echo "  Removed completely: ${#REMOVED[@]}   Kept (unregistered): ${#KEPT[@]}   Restored: ${#RESTORED[@]}"
echo "Restore report: $REPORT"
echo "Restart your AI client(s) to apply the change."
