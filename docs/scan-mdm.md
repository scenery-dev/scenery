# Scheduled re-scans from an MDM

`scripts/scan-mdm.sh` (served at `https://install.scenery.dev/scan-mdm.sh`) lets an MDM re-run the runnerless scan on a schedule. It runs as root, finds the person logged in at the console, and runs the scan as that user. It installs nothing and schedules nothing on the device. The MDM owns the schedule.

## Token

Create an org scan token in the portal (Settings, scan tokens). Store it in the MDM as a secret parameter. The script reads `SCENERY_SCAN_TOKEN` from its environment and passes it on through the environment only (`sudo --preserve-env=SCENERY_SCAN_TOKEN`), so it never appears in `ps` output.

## What it does

1. Finds the console user (`stat -f%Su /dev/console` on macOS, `loginctl` on Linux).
2. If nobody is logged in, or the user is `root` or `loginwindow`, prints `no user logged in; nothing scanned` and exits 0. The next scheduled run tries again.
3. Downloads `scan.sh` over HTTPS and runs it as that user (`launchctl asuser <uid> sudo -u <user>` on macOS). The scan refuses to run as root and needs the user's own home folder.
4. Returns the scan's exit code.

Extra arguments go to the scan, so `scan-mdm.sh --dry-run` shows what would be reported without sending it.

## Monthly schedule

### Jamf

1. Add `scan-mdm.sh` under Settings, Computer management, Scripts. Name the token parameter `Parameter 4`.
2. Create a policy that runs the script with the token in Parameter 4.
3. Trigger: Recurring check-in. Frequency: Once per month per computer.

Jamf passes parameters as positional arguments, so use a one-line payload script that exports the token and calls the wrapper:

```sh
#!/bin/sh
export SCENERY_SCAN_TOKEN="$4"
curl --proto '=https' --tlsv1.2 -fsSL https://install.scenery.dev/scan-mdm.sh | sh
```

### Kandji

Add a Custom Script (Library), execution frequency "Run every 30 days" . Use the same payload, with the token from your Kandji secret instead of `$4`:

```sh
#!/bin/sh
export SCENERY_SCAN_TOKEN="<token from your secret store>"
curl --proto '=https' --tlsv1.2 -fsSL https://install.scenery.dev/scan-mdm.sh | sh
```

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Scan uploaded, or nothing to do (nobody logged in, or the Scenery runner is already installed on the machine). |
| 2 | Refused or not authorised: no token, bad or revoked token, or run as root. |
| 3 | The upload failed. The report was saved to a file; see below. |
| 130 | Interrupted (interactive sign-in only; not used with a token). |

## When an upload fails (exit 3)

The scan prints `Upload failed: ... Report saved to <path>`. The report is written as the scanned user into the system temp directory, named `scenery-scan-<timestamp>-<random>.json`, with mode 0600 (owner only). The random suffix means you cannot guess the name, so read the path from the MDM script log. Retry on the device with `scenery scan --once --from-file <path>`, or collect the file and send it to IT. The next monthly run starts a fresh scan.
