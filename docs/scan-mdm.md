# Scheduled re-scans from an MDM

`scripts/scan-mdm.sh` lets an MDM re-run the runnerless scan on a schedule. It runs as root, finds the person logged in at the console, and runs the scan as that user. It installs nothing and schedules nothing on the device. The MDM owns the schedule.

Status: the sudo and launchctl hand-off is covered by stub tests only. It must be verified on a VM (macOS and Linux) before the pilot.

## Token

Create an org scan token in the portal (Settings, scan tokens). Use a short-lived token and revoke it when the campaign ends. A scan token can only post scan reports, so a leaked one is low impact.

Be clear about where the token lives. Jamf script parameters and Kandji script bodies are not secret stores: the token is visible to MDM admins, and Jamf hands `$4` to the script as a command-line argument, so it is briefly visible in the process list on the device. This is why the token should be short-lived and revocable.

On the device, the wrapper never puts the token on a command line. It writes it to a mode 0600 file in a private work directory owned by the console user, runs the scan with `--token-file <path>`, and deletes the file when it exits.

## What it does

1. Finds the console user (`stat -f%Su /dev/console` on macOS, an active graphical `loginctl` session on Linux).
2. If nobody is logged in, or the user is `root`, `loginwindow` or `_mbsetupuser`, prints `no user logged in; nothing scanned` and exits 0. A headless Linux machine (no graphical session) also exits 0 without scanning. The next scheduled run tries again.
3. Downloads `scan.sh` over HTTPS and runs it as that user (`launchctl asuser <uid> sudo -u <user>` on macOS). The scan refuses to run as root and needs the user's own home folder.
4. Returns the scan's exit code.

Extra arguments go to the scan, so `scan-mdm.sh --dry-run` shows what would be reported without sending it.

## Installing the wrapper (no `curl | sh` as root)

Do not pipe a download into a root shell. Use one of these:

- Paste the contents of `scan-mdm.sh` from the release you have reviewed into the MDM script body. This pins the version.
- Or download the script and verify it against the release's SHA256SUMS before running. For example, in the MDM script body:

```sh
#!/bin/sh
set -eu
TAG=v2.8.0
BASE="https://github.com/scenery-dev/scenery/releases/download/${TAG}"
D="$(mktemp -d)"; trap 'rm -rf "$D"' EXIT
curl --proto '=https' --tlsv1.2 -fsSL -o "$D/scan-mdm.sh" "${BASE}/scan-mdm.sh"
curl --proto '=https' --tlsv1.2 -fsSL -o "$D/SHA256SUMS" "${BASE}/SHA256SUMS"
want="$(awk '$2=="scan-mdm.sh"{print $1}' "$D/SHA256SUMS")"
got="$(shasum -a 256 "$D/scan-mdm.sh" | awk '{print $1}')"
[ -n "$want" ] && [ "$want" = "$got" ] || { echo "scan-mdm.sh checksum mismatch" >&2; exit 1; }
SCENERY_SCAN_TOKEN="$4" sh "$D/scan-mdm.sh"
```

(Replace `"$4"` with however your MDM supplies the token.) The release must publish `scan-mdm.sh` next to SHA256SUMS; check this before relying on the form above.

## Schedule

- Jamf: create a policy that runs the script with the token in Parameter 4. Trigger: Recurring check-in. Frequency: Once per month per computer.
- Kandji: a Custom Script can run once or daily. Kandji has no monthly option, so for a monthly cadence either use a Jamf policy or have the script itself skip when it already ran this month (an MDM-side schedule). Running it daily is harmless but wasteful.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Scan uploaded, or nothing to do (nobody logged in, headless Linux, or the Scenery runner is already installed). |
| 1 | `scan.sh` could not be downloaded, or the scan itself failed to start (for example an unsupported platform). |
| 2 | Refused or not authorised: no token, bad or revoked token, or run as root. |
| 3 | The upload failed. The report was saved to a file; see below. |
| 130 | Interrupted (interactive sign-in only; not used with a token). |

## When an upload fails (exit 3)

The scan prints `Upload failed: ... Report saved to <path>`. The report is written as the scanned user into the system temp directory, named `scenery-scan-<timestamp>-<random>.json`, with mode 0600. The name has a random suffix, so read the path from the MDM script log. Retry on the device, as the user, through `scan.sh`: `sh scan.sh --token-file <token file> --from-file <path>` (or without `--token-file` to sign in with `--login`). Or collect the file and send it to IT. The next scheduled run starts a fresh scan.
