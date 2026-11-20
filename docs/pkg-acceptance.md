# macOS pkg acceptance (unsigned, 0.0.0-dev)

Built 2026-11-20 from the macos-pkg worktree. Verified on the dev Mac without root; every sudo/VM item is PENDING for the user.

## Verified (no root)

- **Build:** `cargo build --release --target aarch64-apple-darwin --target x86_64-apple-darwin` succeeded.
  `scripts/build-pkg.sh --flavor prod|dev 0.0.0-dev <arm64> <x86_64> target/pkg-acceptance` wrote:
  - `target/pkg-acceptance/scenery-0.0.0-dev-macos.pkg` (10.57 MB, unsigned: `pkgutil --check-signature` = "no signature")
  - `target/pkg-acceptance/scenery-0.0.0-dev-macos-dev.pkg` (10.57 MB)
- **Payload (both):** `/Library/Application Support/Scenery/bin/scenery` (100755, `lipo -archs` = `x86_64 arm64`), `uninstall.sh` (100755), `/Library/LaunchAgents/dev.scenery.agent.plist` (100644), all root:wheel. Scripts: `preinstall`, `postinstall` (`SCENERY_PKG_FLAVOR=prod` / `dev` as built).
- **Plist:** Label `dev.scenery.agent`, ProgramArguments `/Library/Application Support/Scenery/bin/scenery daemon run --foreground`, RunAtLoad, KeepAlive, LimitLoadToSessionType Aqua, Umask 63 (0077).
- **Choices:** prod pkg has only hidden `runner`. Dev pkg: `prod` (visible, disabled, selected unless dev is selected), `dev` (visible, off by default), `runner` (hidden). Dev choice payload `registry-url` = `https://dev.scenery.dev`; prod choice has a postinstall only (no payload).
- **AppleDouble (fixed in `build-pkg.sh`):** the component BOM used to contain 163-byte AppleDouble `._*` entries (Bom size 0), caused by the `com.apple.provenance` xattr on the source files. `scrub_appledouble` now rewrites a component pkg without them, only when its Bom, Payload or Scripts actually contain `._` entries; all `pkgbuild` calls pin `--compression legacy`. `scripts/test_build_pkg.sh` asserts that none remain in the payload, the Bom or the Scripts of either flavor. Still worth a check after install that no stray `._*` files sit next to the binary.
- **(h) Resource budget, arm64 release binary, daemon in the foreground (no registry login), temp HOME `/tmp/sn.9wEH`, registry `http://127.0.0.1:9` (unreachable) with `SCENERY_ALLOW_INSECURE_REGISTRY=1`, 600 s idle, sampled every 15 s via `ps -o rss=,time=`:**
  - RSS: start 19,008 kB; peak 20,288 kB (19.8 MB); mean ~19.9 MB. Budget 50 MB: PASS.
  - CPU time: 0:00.04 at t=30 s, 0:00.24 at t=600 s; delta 0.20 s over 570 s = 0.035% (budget < 0.6 s / 0.1% over 10 min): PASS. The 60 s poll is included. The brew 2.7.0 baseline was ~28 MB / 0.03%.
  - Caveat: this run was NOT pkg-managed (binary not at `PKG_BIN`, no system plist), so it is brew-mode behavior. The registry was unreachable, so sync paths were idle. Re-measure the real pkg-managed daemon (item h below).
- **Poll wiring of a new Cursor config (VERIFIED at runtime, 2026-11-20, brew-mode `FirstTimeOnly` policy):** release binary, `HOME=/tmp/scn.JOlc`, `SCENERY_REGISTRY_URL=https://127.0.0.1:9`. The first tick (baseline, 12:22:55) wired nothing (no clients). At 12:23:02 I created `$HOME/.cursor/mcp.json` = `{}`; read back at 12:24:17 it held a `mcpServers.scenery` entry (`command` = the release binary, `args` = `[mcp, serve]`). The daemon log shows `wired Scenery MCP entry into AI clients clients=["Cursor"]` at 17:23:55Z, about 53 s after the file was created, inside the 60 s poll. The earlier note that it stayed `{}` was wrong: a client created after the baseline IS wired on the next tick. Idle RSS at ~100 s: 20,064 kB. Still to confirm on a real pkg install (`Enforce` policy).
- The measured daemon was stopped by PID. The brew daemon was untouched. `target/` is gitignored.

## PENDING — user (needs sudo/VM)

Setup: fresh macOS VM (UTM/Tart) with two local users; `cd scenery-backend && ./devbox.sh start`. Copy both pkgs in.

1. **Dev flavor, Customize screen:** double-click `scenery-0.0.0-dev-macos-dev.pkg`, then Customize. Confirm Production is selected and greyed out. Tick Dev, and Production should untick. Untick Dev, and Production should reselect. Install with Dev, then confirm `cat "/Library/Application Support/Scenery/registry-url"` = `https://dev.scenery.dev` and `~/Library/Application Support/Scenery/logs/scenery.log` (or the daemon log) shows requests to `dev.scenery.dev`. Repeat with Prod, where the file must be absent.
2. **Managed pref override on the prod pkg:**
   ```
   sudo defaults write "/Library/Managed Preferences/dev.scenery.runner" RegistryURL https://dev.scenery.dev
   sudo installer -pkg scenery-0.0.0-dev-macos.pkg -target /
   ```
   Daemon must talk to dev.scenery.dev.
3. **(a)** `launchctl print gui/$(id -u)/dev.scenery.agent` shows running; log in as user 2 and check `launchctl print gui/<uid2>/dev.scenery.agent`.
4. **(b)** `~/.claude.json` has the Scenery HTTP entry with Claude Code installed. Install Claude Code afterward and the entry must appear within 60 s. Also create `~/.cursor/mcp.json` = `{}` and confirm it is wired within 60 s (verified in brew mode above; repeat under the pkg).
5. **(c)** Authenticate in Claude Code (`/mcp`), reboot, and it must still be signed in.
6. **(d)** Delete the Scenery entry from `~/.claude.json`; it must return within 60 s.
7. **(e) Upgrade:** rebuild with version `0.0.1-dev`, then run `sudo installer -pkg scenery-0.0.1-dev-macos.pkg -target /`. Then `ps -o command= -p $(pgrep -f "scenery daemon run")` must show the new binary, and `scenery --version` must report `0.0.1-dev`. Confirm only one daemon is running.
8. **(f) Brew coexistence:** on a VM with `brew install scenery-dev/tap/scenery` run first, install the pkg. Only `dev.scenery.agent` should be running (`ps aux | grep "daemon run"`, `brew services list`), and client entries must point at `/Library/Application Support/Scenery/bin/scenery`.
9. **(g) Uninstall:** `sudo "/Library/Application Support/Scenery/uninstall.sh"`. The agent, binary, `/usr/local/bin/{scenery,scn}` symlinks and install root are gone, client configs are restored, and the portal still shows the device as paired. Check for no leftover `._*` files.
10. **(h) Resource budget on the real pkg daemon (hard gate):** after 10 idle minutes, with the pkg-managed daemon PID from `launchctl print`, `ps -o rss=,time= -p <pid>` at start and end must show RSS < 50 MB and a CPU-time delta < 0.6 s. Do this once signed in and paired (a live registry session), because the measurement above ran with an unreachable registry.
11. **Step 4, this Mac:** `brew services stop scenery`, repeat (a), (b), (e), then `brew services start scenery`.


## Managed preferences scope

Only the device-level managed preferences file, `/Library/Managed Preferences/dev.scenery.runner.plist` (domain `/Library/Managed Preferences/dev.scenery.runner`), is read for `RegistryURL`. User-scoped managed profiles (`/Library/Managed Preferences/<user>/dev.scenery.runner.plist`) are NOT read. Deploy the MDM configuration profile to the device (system) scope.
