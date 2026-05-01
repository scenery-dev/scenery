---
name: M2.2 acceptance gate results
description: M2 gate outcome, integration test patterns, and AC wiring notes from stream M2.2
type: project
---

M2.2 delivered at `3b07e5e` (on branch `m2/stream-2-gate`, worktree `vectorhawkd-worktrees/m2-stream-2-gate`). Gate ran clean: 7 PASS + 1 N/A (AC2 Linux is N/A on Darwin).

**Why:** Final M2 stream — verification scaffolding after M2.1 landed install handlers.

**How to apply:** Tag `m2-complete` once this branch merges to main.

## Integration test locations

- `crates/vectorhawkd-cli/tests/m2_install_macos.rs` — `#[cfg(target_os = "macos")]`, `#[ignore]`
- `crates/vectorhawkd-cli/tests/m2_install_linux.rs` — `#[cfg(target_os = "linux")]`, `#[ignore]`

Both use `CARGO_MANIFEST_DIR` to locate `../../target/release/vectorhawk` (same pattern as m1_multi_shim.rs).

## AC wiring confirmed

- **AC5**: `install::ensure_installed` is called via `tokio::task::spawn_blocking` inside `cmd_mcp_setup` in `crates/vectorhawkd-cli/src/main.rs` at line ~891. Source grep on `ensure_installed` + `install::status` in main.rs is a reliable gate check.
- **AC6**: `vectorhawk doctor` outputs `Daemon install:  <state>` (two spaces after colon). Grep pattern `^Daemon install:` is reliable.

## macOS-specific notes

- After `daemon uninstall`, the socket at `~/Library/Application Support/VectorHawk/agent.sock` disappears within ~1 s in practice (launchctl bootout is synchronous for the process stop). The 3 s budget in the test is comfortable.
- `launchctl kickstart -k` on macOS 15+ Sequoia means the socket appears quickly after install; the 5 s budget is generous (usually < 2 s observed).

## Gate script AC3/AC4 derivation

AC3 (uninstall clean) and AC4 (idempotency) are derived from the install test result rather than run as independent checks. This avoids a second full install/uninstall cycle in the gate. AC3 = steps 7-9 of install test. AC4 = step 6 of install test.
