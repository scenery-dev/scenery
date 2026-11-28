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
3. Downloads `scan.sh` from the GitHub release for its tag and checks it. The `scan-mdm.sh` published with a release (and served at `install.scenery.dev/scan-mdm.sh`) is stamped with that release's tag and the SHA-256 of that release's `scan.sh`, so it checks `scan.sh` with `shasum -a 256` (macOS) or `sha256sum` (Linux) and needs nothing else. When `minisign` or a signed-in `gh` is on `PATH`, it also checks `scan.sh` against the release's `SHA256SUMS` and verifies that file's signature. The stamped `scan.sh` then checks the scan binary the same way. If any download or check fails, it refuses to run and exits 1.
4. Runs `scan.sh` as that user (`launchctl asuser <uid> sudo -H -u <user>` on macOS). The scan refuses to run as root and needs the user's own home folder. The scan uses the binary from the same release tag.
5. Returns the scan's exit code.

Extra arguments go to the scan, so `scan-mdm.sh --dry-run` shows what would be reported without sending it.

Requirements on the device: `curl` and `shasum` (both ship with macOS) or `sha256sum` (Linux). `minisign` and `gh` are optional. The stamped wrapper runs its own tag by default. Set `SCENERY_SCAN_VERSION` only to that same tag: any other tag has no pinned hash, so the wrapper then requires `minisign` or a signed-in `gh` on root's `PATH` (as the unstamped copy in the repo always does) and refuses without one. MDM scripts often run with a minimal `PATH` that leaves out Homebrew, so set it in the script body if you rely on minisign, for example `PATH=/opt/homebrew/bin:/usr/local/bin:$PATH`.

`sudo -H` resets the environment. The wrapper forwards `SCENERY_SCAN_VERSION` and `SCENERY_REGISTRY_URL` to the scan explicitly (neither is a secret), so setting them in the wrapper's environment works. You can also pass the backend as an argument: `scan-mdm.sh --registry-url https://dev.scenery.dev`. Use one of these for any target other than production. Other variables do not reach the scan.

## Installing the wrapper (no `curl | sh` as root)

Do not pipe a download into a root shell. Use one of these:

- Paste the contents of `scan-mdm.sh` from the release you have reviewed (the release asset, which is stamped, not the repo copy) into the MDM script body. It pins its own tag and the hash of that tag's `scan.sh`, so `scan.sh` and the scan binary come from the same release and no `minisign` is needed on the device.
- Or download the script and verify it before running. Each release from v2.8.0 publishes `scan.sh`, `scan-mdm.sh`, `install.sh`, `SHA256SUMS` (which lists them) and `SHA256SUMS.minisig`. Verify the signature on `SHA256SUMS` on your own machine, then the checksum of `scan-mdm.sh`, and pin that checksum in the MDM script. For example, after checking the signature once with minisign, in the MDM script body:

```sh
#!/bin/sh
set -eu
TAG=v2.8.0
# sha256 of scan-mdm.sh from that release's SHA256SUMS, after you verified its
# signature: minisign -Vm SHA256SUMS -x SHA256SUMS.minisig -P RWREIYd8cL30cy+6WHSsE+0oVDsfoPu/1BmCGX4rb0Fgs4+ZC3jjW/gp
WANT=<sha256 of scan-mdm.sh>
D="$(mktemp -d)"; trap 'rm -rf "$D"' EXIT
curl --proto '=https' --tlsv1.2 -fsSL -o "$D/scan-mdm.sh" \
  "https://github.com/scenery-dev/scenery/releases/download/${TAG}/scan-mdm.sh"
got="$(shasum -a 256 "$D/scan-mdm.sh" | awk '{print $1}')"
[ "$WANT" = "$got" ] || { echo "scan-mdm.sh checksum mismatch" >&2; exit 1; }
SCENERY_SCAN_TOKEN="$4" sh "$D/scan-mdm.sh"
```

(Replace `"$4"` with however your MDM supplies the token.) The stamped wrapper fetches `scan.sh` and the scan binary from its own tag and checks both against hashes pinned at release time, so the one hash in the MDM script covers the whole chain. The public key is the release signing key (minisign key id `73F4BD707C872144`), the same one `install.sh` and `scan.sh` use.

## Schedule

- Jamf: create a policy that runs the script with the token in Parameter 4. Trigger: Recurring check-in. Frequency: Once per month per computer.
- Kandji: a Custom Script can run once or daily. Kandji has no monthly option, so for a monthly cadence either use a Jamf policy or have the script itself skip when it already ran this month (an MDM-side schedule). Running it daily is harmless but wasteful.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Scan uploaded, or nothing to do (nobody logged in, headless Linux, or the Scenery runner is already installed). |
| 1 | `scan.sh` could not be downloaded or verified (pinned checksum, signature, or an unpinned tag with no minisign/gh), or the scan itself failed to start (for example an unsupported platform). |
| 2 | Refused or not authorised: no token, bad or revoked token, or run as root. |
| 3 | The upload failed. The report was saved to a file; see below. |
| 130 | Interrupted (interactive sign-in only; not used with a token). |

## When an upload fails (exit 3)

The scan prints `Upload failed: ... Report saved to <path>`. The report is written as the scanned user into the system temp directory, named `scenery-scan-<timestamp>-<random>.json`, with mode 0600. The name has a random suffix, so read the path from the MDM script log. Retry on the device, as the user, through `scan.sh`: `sh scan.sh --token-file <token file> --from-file <path>` (or without `--token-file` to sign in with `--login`). Or collect the file and send it to IT. The next scheduled run starts a fresh scan.
