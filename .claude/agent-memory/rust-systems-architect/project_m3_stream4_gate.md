---
name: M3.4 gate + concurrency tests
description: M3.4 final stream: acceptance gate, concurrency tests, doctor OAuth listener line — key patterns and gotchas
type: project
---

M3.4 delivers the M3 acceptance gate, two concurrency integration tests, and the `OAuth listener:` doctor line.

**Why:** Verification scaffold for M3 OAuth flows; gate verifies AC1-AC8 against live daemon binary.

**How to apply:** When reading or extending the M3 gate pattern for M4+, note these decisions:

## macOS `timeout` absence
macOS does not ship GNU `timeout`. The gate uses a background-process + manual kill pattern instead:
```bash
"${BIN}" args >"${TMPOUT}" 2>&1 &
BG_PID=$!
elapsed=0
while kill -0 "${BG_PID}" 2>/dev/null && [[ ${elapsed} -lt 10 ]]; do
    sleep 0.5; elapsed=$((elapsed+1))
done
wait "${BG_PID}"; CODE=$?
```
This is portable to both macOS and Linux. Do NOT use `timeout 5 cmd` in gate scripts.

## Doctor OAuth listener wiring pattern
`probe_oauth_listener_port` is a self-contained `#[cfg(unix)]` async helper in `cmd_doctor` that:
1. Connects to the daemon socket with 500 ms timeout.
2. Sends `auth/get_oauth_listener_port` JSON-RPC (4-byte big-endian length prefix).
3. Reads the response with a 500 ms timeout.
4. Returns `"running on port N"` / `"running on port N (fallback)"` / `"not running"`.
The fallback port label fires when the bound port != 39127 (the base).

## Concurrency test location
`crates/vectorhawkd-cli/tests/m3_concurrency.rs` — two `#[ignore]` tests:
- `m3_concurrent_login_flows_no_cross_contamination`: fires B-callback before A-callback to confirm no positional coupling.
- `m3_concurrent_same_state_second_subscriber_errors`: verifies `OAuthState::DuplicateSubscriber` path over JSON-RPC.

Both tests use the same binary-subprocess + FramedSocket pattern as `m3_oauth_listener.rs` and `m3_login_e2e.rs`.

## RSS at M3-complete
Daemon idle RSS measured 8.6 MB on macOS arm64 (budget: 50 MB). The axum OAuth listener adds negligible overhead.

## Pre-existing clippy issues (14 errors, not regressions)
At M3 HEAD, `cargo clippy --all-targets --all-features -- -D warnings` reports 14 errors, all in pre-existing integration test files (`m0_acceptance.rs`, `m0_daemon_kill.rs`, `m1_multi_shim.rs`, `m1_blocking_io_stress.rs`, `m2_install_macos.rs`). These are `&PathBuf` instead of `&Path` and doc formatting issues in test helper functions. All pre-date M3.4 work; the CLI crate itself is clean.

## Gate exit codes
`m3_acceptance.sh` returns 0 = all pass, 1 = any fail. AC7 (M0/M1/M2 regressions) runs first with fast-abort on failure.
