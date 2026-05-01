---
name: M3.3 CLI PKCE flow and daemon refresh loop
description: M3.3 implementation notes: PKCE primitives, CLI login rewrite, daemon refresh task
type: project
---

M3.3 landed on branch `m3/stream-3-cli-flow` (commit d4d6f32).

**PKCE primitives (vectorhawkd-core/src/auth.rs):**
- `generate_code_verifier()` uses double-UUID → SHA-256 → base64url-no-pad to produce 43-char verifier without adding a `rand` dep
- `derive_code_challenge(verifier)` is public — used in tests and by callers; passes RFC 7636 Appendix B test vector
- `OAuthInitiation` gained a `code_challenge` field
- `exchange_oauth_code` now POSTs form-urlencoded (not JSON) to `/portal/auth/cli/token`
- `needs_refresh` decodes JWT payload manually (no `jsonwebtoken` dep); returns true conservatively on parse failure
- `load_all_tokens` added for daemon refresh loop to iterate all rows
- `base64 = "0.22"` added to workspace deps

**CLI cmd_auth_login rewrite (vectorhawkd-cli/src/main.rs):**
- Uses inline async helpers `send_rpc`/`recv_rpc` (copy of write_framed/read_framed semantics) — decided not to import from vectorhawkd-mcp to avoid coupling the framing format dependency into the CLI binary at link time
- 2s connect timeout via `tokio::time::timeout` before `UnixStream::connect`
- Exits code 2 (not code 1) on daemon-not-running — intentional so scripts can distinguish daemon errors
- No stdin prompt whatsoever

**Daemon refresh loop (vectorhawkd-daemon/src/lib.rs):**
- `refresh_one_tick(state, registry_url)` is `pub` so tests can drive it directly without the 60s interval
- The `registry_url` param is kept for future rate-limit context but the actual HTTP calls use each row's own stored `registry_url`
- Spawns after the registry sync task; skips the immediate first tick (same pattern as sync loop)

**Why:** M3.1 spec compliance + AC2/AC4/AC8 from RUN1_M3_STREAMS.md.

**How to apply:** When working on M3.4 gate or concurrency tests, the full flow can be tested by combining the M3.1 `FramedSocket` helper with `refresh_one_tick` driven directly — no binary subprocess needed for most tests.
