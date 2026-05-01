---
name: M3.1 OAuth callback listener
description: M3.1 delivery notes: axum listener, OAuthState pub/sub, DaemonContext, integration test pattern, reqwest::blocking drop gotcha
type: project
---

M3.1 delivered at `bdf37b7` (branch `m3/stream-1-listener`). 29 unit tests + 2 integration tests added; all M0/M1/M2 gates pass.

**Why:** Adds real OAuth PKCE flow infrastructure to the daemon — browser redirect receiver and CLI notification channel — so M3.3 can replace the paste-the-code login stub.

**How to apply:** M3.3 should call `auth/get_oauth_listener_port` then `auth/wait_for_callback` via the Unix socket; no code changes to the daemon needed.

## Key decisions

- **axum 0.8** over raw hyper: ergonomic enough, adds ~300 KB dep weight to daemon only (shim does NOT link axum, verified by `vectorhawkd-shim/Cargo.toml` using `default-features = false` on vectorhawkd-mcp).
- **oneshot channel** in `OAuthState`: each subscribe/notify pair is a single delivery, so oneshot is exactly right. `cancel_all` drops all senders on daemon shutdown.
- **DaemonContext struct**: added to `socket_dispatch.rs` to carry `oauth_state: Arc<OAuthState>` and `listener_port: Option<u16>` into per-connection handlers without N separate args.
- **Port range 39127..=39136**: matches what the registry validates as acceptable `redirect_uri` hosts. Daemon continues without listener if all ports taken.

## Integration test gotcha: reqwest::blocking drop in async context

Attempted in-process `run_daemon` inside `#[tokio::test]` — panics with "Cannot drop a runtime in a context where blocking is not allowed". Root cause: `RegistryClient` holds a `reqwest::blocking::Client` which contains an internal Tokio runtime; dropping it inside an async task causes the panic.

Fix: use the same subprocess binary pattern as m0_acceptance/m1_multi_shim. Integration test spawns the release `vectorhawkd` binary as a child process and talks to it via a sync `FramedSocket` (4-byte big-endian length-prefix framing). The `auth/wait_for_callback` call (which blocks until browser fires) is issued from a background `std::thread` while the main test thread sends the HTTP callback.

## http_get helper in integration test

No reqwest in the integration test (to avoid the async runtime issue). Uses a raw `std::net::TcpStream` with manual HTTP/1.1 framing. axum's server speaks HTTP/1.1, so `GET /oauth/cli/callback?code=...&state=...` works fine.

## Daemon idle RSS after M3.1

Measured 8.4 MB (was ~8 MB pre-M3.1). axum adds ~0.4 MB. Well within the 50 MB budget.
