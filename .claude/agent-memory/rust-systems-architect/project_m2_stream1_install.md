---
name: M2 Stream 1 install module API surface
description: M2.1 install module structure, launchctl quirk on macOS 15+, and key API surface for M2.2 to test against
type: project
---

M2.1 is complete on branch `m2/stream-1-packaging`. The install module lives at `crates/vectorhawkd-cli/src/install/{mod.rs,macos.rs,linux.rs}`.

**Why:** M2 ships `vectorhawk daemon install/uninstall` for LaunchAgent (macOS) and systemd-user unit (Linux) so the daemon auto-starts at user login. `mcp setup` now calls `ensure_installed()` before writing AI client config.

**How to apply:** M2.2 (gate + integration tests) should test against this API surface.

## Key API surface (`install` module public items)

- `install::install() -> Result<()>` — installs and starts; idempotent
- `install::uninstall() -> Result<()>` — stops and removes; idempotent
- `install::ensure_installed() -> Result<()>` — no-op if already running
- `install::status() -> Result<InstallStatus>` — `NotInstalled` / `InstalledNotRunning { unit_path }` / `InstalledAndRunning { unit_path }`
- `install::daemon_socket_path() -> String` — platform socket path without bootstrapping AppState
- `install::socket_is_reachable(path, timeout_ms) -> bool` — blocking probe

## macOS launchctl quirk (Sequoia / macOS 15+)

`launchctl bootstrap gui/<uid> <plist>` defers the initial spawn as "speculative" even with `RunAtLoad=true`, leaving the agent in `state = not running`. Fix: add `launchctl kickstart -k gui/<uid>/com.vectorhawk.agent` after bootstrap. This is in `macos.rs` step 7.

Without kickstart, the smoke test shows:
- `launchctl print` → `state = not running`, `pended nondemand spawn = speculative`
- After `launchctl kickstart -k …` → `state = running`, socket appears

## File paths

- macOS plist: `~/Library/LaunchAgents/com.vectorhawk.agent.plist`
- macOS log dir: `~/Library/Logs/VectorHawk/` (stdout.log + stderr.log)
- Linux unit: `~/.config/systemd/user/vectorhawk-agent.service`
- Linux autostart fallback: `~/.config/autostart/vectorhawk.desktop`

## mcp setup integration

`cmd_mcp_setup` checks `install::status()` before writing AI client config. If not running, calls `ensure_installed()` then `wait_for_daemon_socket(5_000)`. `--dry-run` skips this entirely.

## Commits

- `b6d6d6a` feat(install): macOS LaunchAgent install/uninstall handlers
- `eae3262` fix(install): add launchctl kickstart to force immediate daemon start on macOS 15+

## Gate results at tip

M0: 6/6 PASS, M1: 12/12 PASS, 226 unit tests pass.
