# Runnerless Scan (runner) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `scenery scan --once` reads every MCP server, skill and plugin on the machine, sanitizes them with the shared golden rules, uploads one report to `POST /scan/reports`, prints a summary and exits. It leaves no daemon, LaunchAgent, client-config edit, state directory or stored credential behind.

**Architecture:** The scan reuses the daemon's collectors through a new seam that takes an in-memory SQLite connection instead of `state.db`, so discovery code stays in one place and nothing is written. It authenticates with an org scan token from the environment or with an in-process browser login (`--login`) whose session it revokes before exiting. A new `scenery_core::scan_identity` module derives the device identity. The daemon also sends it on `POST /devices/register`, so a later runner links to the scan-only device.

**Tech Stack:** Rust workspace (`scenery-core`, `scenery-daemon`, `scenery-cli`), tokio current-thread runtime, reqwest, rusqlite, mockito for HTTP tests, POSIX sh for `scan.sh`.

**Spec:** `/Users/shadowduck/code/scenery/context/2026-11-21-runnerless-pilot-design.md`. The backend wire contract and the rulings on the builder-left items are in `/Users/shadowduck/code/scenery/scenery-backend/docs/superpowers/plans/2026-11-25-runnerless-scan.md`, "Rulings" and Task 4. Read both.

**Execution order:** Start after backend Tasks 1–8 are deployed to dev. Portal Tasks 9–10 follow this plan.

## Global Constraints

- **Release gate (binding).** Release 2.8.0 ships only if the daemon's release build stays under the budget: idle CPU under 0.6 s per 10 min, and RSS under 50 MB. Measure with `cargo test -p scenery-daemon --release --lib idle_cost -- --ignored --nocapture --test-threads=1` and `scripts/measure_daemon_rss.sh`. A scan run must leave no process resident: after it exits, `pgrep -f 'scenery scan'` is empty and no child (`ioreg`, browser opener) survives.
- **Zero footprint.** `scenery scan` never calls `AppState::bootstrap`, `save_tokens`, any keychain API or `secret_file`. It never writes under `AppState::resolve_root_dir()` or into any AI client config, and never runs `tracked::reconcile`. It writes exactly one file, and only on a failed upload or with `--out`: the report.
- **No secrets leave the machine.** MCP args go through `mcp_args::redact_args` (golden file `crates/scenery-daemon/src/managed_paths/mcp_args_golden.json`, unchanged). Args are included only when `GET /scan/session` returned `auto_submit_mcp: true`. Env and header values are never read into the report. No skill body is uploaded; skill items carry the hash and the classifier `detail` only. Every `source_path` is `~`-masked.
- **The token never goes in argv.** The org scan token comes from `SCENERY_SCAN_TOKEN` or `--token-file <path>`, never from a flag value.
- **Version gating.** Workspace version 2.8.0. The backend answers 426 to `/scan/*` below 2.8.0. The daemon's extra `scan_identity` register field is ignored by older backends, since Pydantic ignores unknown fields. Check this against prod before release.
- **Wire contract (verbatim from the backend plan, Task 4):** `format` is `"scenery-scan-report/1"`; `GET /scan/session` returns `{org_name, attribution, user_email, auto_submit_mcp, max_items}`; `POST /scan/reports` returns `{device_id, accepted, deduped, rejected, mcp_submitted, inventory_url}`; the error codes are `scan_token_invalid`, `scan_token_revoked`, `scan_token_expired`, `too_many_items`, `unsupported_format`, `runner_present` and `scan_device_revoked`.
- **Exit codes:** 0 means uploaded, or the runner is present. 2 means a usage or config problem (root, no auth, bad flags). 3 means the upload did not complete and the report was saved.
- **Conventions.** `cargo clippy` and `cargo fmt` are clean before each commit. Tests that touch `HOME` or env hold `managed_paths::ENV_MUTEX`. Commit to `main`. Messages carry no Claude trailers. The 9–5 weekday rule is lifted until 2026-11-30.

## Review Focus

1. **The scan runs as root from an MDM**, so `HOME` is `/var/root` and nothing is found. Expected: it refuses with exit 2 and "Run the scan as the logged-in user, not root (see scan-mdm.sh)". Owner: Task R3, `scan_refuses_root`.
2. **The runner is already installed** (the state dir has `state.db`). Expected: it prints "This device runs the Scenery runner, which already reports it. No scan needed.", exits 0 and uploads nothing. Owner: Task R3, `scan_skips_when_runner_present`.
3. **The token is revoked or expired between the session check and the upload, or the network drops.** Expected: the report is saved with mode 0600, the path is printed, the reason names the token code, and the exit code is 3. Owner: Task R4, `upload_failure_saves_report`.
4. **A machine with more items than `max_items`.** Expected: no silent truncation. The report is saved, the message says how many items and the limit, and the exit code is 3. Owner: Task R4, `over_limit_saves_and_fails`.
5. **A scan leaves files behind** (state dir, keychain entry, log, temp file). Expected: a fixture `HOME` is byte-for-byte identical before and after `--dry-run` and an uploaded scan. Owner: Task R6, `scan_leaves_no_footprint`.

---

### Task R1: `scan_identity` in scenery-core; the daemon sends it on register

**Files:**
- Create: `crates/scenery-core/src/scan_identity.rs`, `crates/scenery-core/src/scan_identity_tests.rs`
- Modify: `crates/scenery-core/src/lib.rs`, `crates/scenery-daemon/src/lib.rs:2240-2270` (`register_with_backend` payload)
- Test: `crates/scenery-daemon/src/sync_retry_tests.rs` (register body)

**Interfaces:**
- Produces:
  - `pub enum IdentityBasis { Hardware, Hostname }`, serialized as `"hardware"` or `"hostname"`.
  - `pub struct ScanIdentity { pub hash: String, pub basis: IdentityBasis, pub hostname: String, pub os_username: String }`.
  - `pub fn hash_identity(hardware_id: Option<&str>, hostname: &str, os_username: &str) -> (String, IdentityBasis)`. This is the pure function. With `Some(hw)` it hashes `"scenery-scan-identity-v1\0" + hw.trim().to_uppercase() + "\0" + os_username`. With `None` (or an empty trimmed id) it hashes `"scenery-scan-identity-v1\0host:" + hostname.to_lowercase() + "\0" + os_username`. The output is lowercase hex SHA-256.
  - `pub fn derive() -> ScanIdentity`. On macOS the hardware id is `IOPlatformUUID`, parsed from `ioreg -rd1 -c IOPlatformExpertDevice` (the child is waited on). On Linux it is `/etc/machine-id`, then `/var/lib/dbus/machine-id`. `os_username` comes from `getpwuid(geteuid())`, falling back to `$USER`. `hostname` uses the same lookup as `register_with_backend`.
  - The daemon's register payload gains `"scan_identity": derive().hash`.

- [ ] **Step 1: Write the failing tests** in `scan_identity_tests.rs`:
  - `hash_is_stable_vector`: `hash_identity(Some(" abc-def "), "Host", "alice")` equals the SHA-256 hex of the bytes `b"scenery-scan-identity-v1\0ABC-DEF\0alice"` (compute it in the test with `sha2`), with basis `Hardware`.
  - `hostname_fallback`: for `None` and `Some("  ")`, the basis is `Hostname` and the input is `host:host`.
  - `user_changes_hash`: different `os_username` values give different hashes.
  - `parse_ioreg_line`: `parse_ioreg("    \"IOPlatformUUID\" = \"1234-AB\"\n")` returns `Some("1234-AB")`. Expose `pub(crate) fn parse_ioreg(&str) -> Option<String>`.
  - In `sync_retry_tests.rs`, `register_sends_scan_identity`: a mockito `POST /api/devices/register` matcher requires a JSON body with `scan_identity` matching `^[0-9a-f]{64}$`.
- [ ] **Step 2: Run them to verify they fail.** Run `cargo test -p scenery-core scan_identity && cargo test -p scenery-daemon register_sends_scan_identity`. Expected: FAIL (module missing).
- [ ] **Step 3: Implement the module and add the payload field.** Call `derive()` inside the existing `spawn_blocking`-safe section. Registration runs once per daemon start, so the `ioreg` spawn adds no idle cost.
- [ ] **Step 4: Run them to verify they pass.** Expected: PASS. Run `cargo clippy -p scenery-core -p scenery-daemon`.
- [ ] **Step 5: Commit.** `git commit -m "core: scan identity; daemon sends it on register"`

### Task R2: A storage-free collector seam, and plugin discoveries

**Files:**
- Modify: `crates/scenery-core/src/state.rs` (add `pub fn ephemeral_schema_connection() -> Result<rusqlite::Connection>`), `crates/scenery-daemon/src/managed_paths/discoveries.rs:550-700`, `crates/scenery-daemon/src/managed_paths/mcp_discovery.rs:201-260`, `crates/scenery-daemon/src/managed_paths/mod.rs` (make `scanner` items reachable)
- Create: `crates/scenery-daemon/src/managed_paths/scan_collect.rs`, `crates/scenery-daemon/src/managed_paths/scan_collect_tests.rs`

**Interfaces:**
- Produces:
  - `state::ephemeral_schema_connection()` opens `Connection::open_in_memory()` and applies `SCHEMA_SQL`.
  - `discoveries::collect_discoveries_in(conn: &Connection, roots: &[PathBuf]) -> Vec<DiscoveryItem>`, and `mcp_discovery::collect_mcp_discoveries_in(conn: &Connection, clients: &[ClientConfig], registry_url: &str, include_args: bool) -> Vec<McpDiscoveryItem>`. The existing `collect_discoveries` / `collect_mcp_discoveries` / `collect_all_discoveries` become wrappers that open `state.db` and read the cached policy, as today. Daemon behavior is unchanged.
  - `pub fn mask_home_dir` is re-exported from `mcp_discovery` as `pub(crate)`.
  - `scan_collect::collect_for_scan(home: &Path, registry_url: &str, include_args: bool) -> Result<Vec<DiscoveryItem>>`. It uses an ephemeral connection, `extra_roots()`, `scenery_mcp::setup::detect_ai_clients()`, and plugins from `scanner::scan_plugins_dir` plus `scanner::scan_plugin_marketplaces` over `paths::plugins_dir()`. It masks every `source_path` with `mask_home_dir`. Plugin items have `kind: "plugin"`, `detail = {name, version, description, marketplace}`, string fields from `sanitize_plugin_manifest` only, and the marketplace name for nested layouts. The `local` field is dropped.

- [ ] **Step 1: Write the failing tests** in `scan_collect_tests.rs`. Each builds a temp `HOME` holding a skill in `~/.claude/skills/notes`, a skill in `~/.agents/skills/deploy`, a Cursor `mcp.json` with `{"gh":{"command":"npx","args":["-y","@x/gh","--token","ghp_abcdefghijklmnopqrstuvwxyz0123456789"],"env":{"GH_TOKEN":"secret-value"}}}`, and a plugin `~/.claude/plugins/p1/.claude-plugin/plugin.json`:
  - `collects_all_kinds`: two skills, one MCP item and one plugin are returned.
  - `masks_paths`: every `source_path` starts with `~/`, and none contains the temp dir's absolute path.
  - `no_secret_values`: the serialized items contain neither `ghp_abc…` nor `secret-value`, with `include_args` set to either value. With `include_args = false`, there is no `args` key.
  - `matches_daemon_collector`: the MCP `detail` and `canonical_hash` equal what `collect_all_discoveries` returns for the same fixture with an empty `state.db`, so the golden rules are shared.
  - `writes_nothing`: a recursive listing of `HOME` (path, size, mtime) is identical before and after, and `AppState::resolve_root_dir()` (under that `HOME`) does not exist.
- [ ] **Step 2: Run them to verify they fail.** Run `cargo test -p scenery-daemon scan_collect`. Expected: FAIL.
- [ ] **Step 3: Implement the seam, the wrappers and `collect_for_scan`.**
- [ ] **Step 4: Run them, plus the existing discovery and golden suites.** Run `cargo test -p scenery-daemon managed_paths`. Expected: PASS with no edits to existing tests. `mcp_args_tests` still reads the unchanged golden file.
- [ ] **Step 5: Commit.** `git commit -m "discoveries: storage-free collector seam and plugin items for the scan"`

### Task R3: `scenery scan` command: local scan, report, summary, dry run

**Files:**
- Create: `crates/scenery-cli/src/scan/mod.rs`, `crates/scenery-cli/src/scan/report.rs`, `crates/scenery-cli/src/scan/summary.rs`, `crates/scenery-cli/src/scan/scan_tests.rs`
- Modify: `crates/scenery-cli/src/main.rs` (`Command::Scan(ScanArgs)`, dispatch)

**Interfaces:**
- Consumes: `scan_collect::collect_for_scan` (R2); `scan_identity::derive` (R1).
- Produces:
  - `ScanArgs` (clap): `--once` (required; it is the only mode and is reserved for clarity), `--login`, `--token-file <PATH>`, `--registry-url` (env `SCENERY_REGISTRY_URL`, default `DEFAULT_REGISTRY_URL`, `parse_registry_url`), `--dry-run`, `--json`, `--quiet`, `--out <PATH>` and `--from-file <PATH>`. `--login` conflicts with `--token-file`. `--dry-run` conflicts with `--from-file`.
  - `report::ScanReport { format, scanner_version, scanned_at, device: ReportDevice, items: Vec<DiscoveryItem> }` (serde), matching the backend's `ScanReport` field for field. `report::save(report: &ScanReport, out: Option<&Path>) -> Result<PathBuf>`. The default path is `std::env::temp_dir().join(format!("scenery-scan-{unix_ts}.json"))`. It is created with `create_new` and mode 0600.
  - `summary::render(items: &[DiscoveryItem], quiet: bool) -> String`. The first line is `Found {m} MCP servers, {s} skills, {p} plugins.` Unless `quiet`, a table follows with the columns `KIND  CLIENT  NAME  FLAG`, at most 50 rows sorted by kind then name, then `… and {n} more` when truncated. FLAG is `hold:<reason>` or `suspicious` or blank. CLIENT is `detail.client_name` for MCP, or blank.
  - `scan::run(args: ScanArgs) -> Result<i32>`, which returns the exit code. In order:
    1. Refuse root (`geteuid() == 0`) with exit 2 and the message in Review Focus 1.
    2. If `resolve_root_dir()/state.db` exists, print the runner message and exit 0.
    3. Collect.
    4. With `--dry-run`, print the summary (or `--json`) and exit 0. Upload is Task R4 and login is Task R5. Until R4 lands, a non-dry run returns exit 2 with "upload not available in this build".

- [ ] **Step 1: Write the failing tests** in `scan_tests.rs`. They hold `ENV_MUTEX` and use a temp `HOME` fixture as in R2:
  - `scan_refuses_root`: inject euid through `pub(crate) fn run_with(args, env: &ScanEnv)`, where `ScanEnv { euid: u32, home: PathBuf, now: OffsetDateTime }`. With euid 0, it returns 2 and writes nothing.
  - `scan_skips_when_runner_present`: create `<data dir>/Scenery/state.db`. The result is 0 with the runner message, and the HTTP mock gets zero hits.
  - `dry_run_prints_summary`: the output's first line is `Found 1 MCP servers, 2 skills, 1 plugins.`, and the table has 4 rows.
  - `summary_truncates_at_50`: 60 items give 50 rows plus `… and 10 more`.
  - `json_matches_wire_shape`: `--dry-run --json` output parses, `format == "scenery-scan-report/1"`, and `device.scan_identity` is 64 hex chars.
  - `save_is_private`: `report::save` creates a file with mode `0o600`, and a second save to the same `--out` fails rather than overwriting.
- [ ] **Step 2: Run them to verify they fail.** Run `cargo test -p scenery-cli scan_`. Expected: FAIL.
- [ ] **Step 3: Implement it.** Logging stays `logging::init_cli()` (stderr). `is_daemon_foreground` must stay false for `Scan`.
- [ ] **Step 4: Run them to verify they pass.** Expected: PASS. Run `cargo clippy -p scenery-cli`.
- [ ] **Step 5: Commit.** `git commit -m "cli: scenery scan --once (local scan, summary, dry run)"`

### Task R4: Upload with an org scan token; offline save; `--from-file`

**Files:**
- Create: `crates/scenery-cli/src/scan/upload.rs`, `crates/scenery-cli/src/scan/upload_tests.rs`
- Modify: `crates/scenery-cli/src/scan/mod.rs`

**Interfaces:**
- Consumes: `ScanReport`, `report::save` (R3); the backend `/scan/session` and `/scan/reports` (backend Task 4).
- Produces:
  - `pub enum ScanAuth { Token(String), Bearer(String) }`, whose `header()` returns `Bearer …` in both cases.
  - `upload::session(client: &reqwest::Client, base: &str, auth: &ScanAuth) -> Result<ScanSession, UploadError>`, where `ScanSession { org_name, attribution, user_email: Option<String>, auto_submit_mcp: bool, max_items: usize }`.
  - `upload::post_report(client, base, auth, report: &ScanReport) -> Result<ScanReceipt, UploadError>`, where `ScanReceipt { device_id, accepted, deduped, rejected: Vec<Rejected>, mcp_submitted, inventory_url }`. It retries twice, with 1 s and then 3 s backoff, on connect errors and 5xx only.
  - `UploadError { kind: Auth(code) | TooOld | RunnerPresent | Limit(String) | Network(String) | Server(u16) }`. `Display` gives one plain sentence. For example, Auth `scan_token_revoked` gives "The scan token was revoked by your IT admin."
  - The `run` flow after collecting:
    1. Resolve auth. `SCENERY_SCAN_TOKEN` or `--token-file` (first line, trimmed, must start with `scn_scan_`) gives `Token`. `--login` gives `Bearer` (R5). With neither, exit 2: "Set SCENERY_SCAN_TOKEN or run `scenery scan --once --login`."
    2. Call `session()`. On failure, build the report without args, save it and exit 3.
    3. Collect with `include_args = session.auto_submit_mcp`.
    4. If `items.len() > max_items`, save and exit 3 with "{n} items is over this org's limit of {max}."
    5. Post. On success, print the summary, then `Uploaded to {org_name}: {accepted} new, {deduped} already known. IT reviews them at {inventory_url}.`, then exit 0. `RunnerPresent` prints the runner message and exits 0. Any other error saves the report, prints `Upload failed: {reason}. Report saved to {path} — send it to IT, or retry with: scenery scan --once --from-file {path}`, and exits 3.
  - `--from-file <path>` reads, checks `format`, skips collection and posts with the resolved auth. The file is left in place.
  - `--out <path>` also saves on success, which IT can use to keep a copy.

- [ ] **Step 1: Write the failing tests** in `upload_tests.rs`. They use mockito for `/api/scan/session` and `/api/scan/reports`, and assert that the `X-Scenery-Runner-Version: 2.8.0` header and `Authorization: Bearer scn_scan_…` are sent:
  - `token_upload_happy_path`: exit 0, the receipt line is printed, and no file is written to the temp dir.
  - `args_follow_session_policy`: with `auto_submit_mcp: false`, the posted body has no `args` key. With `true`, `args` is present and redacted per the golden rules (no `ghp_` in the body).
  - `upload_failure_saves_report`: for each of a 401 `scan_token_revoked` body, a 500 three times, and a connection refused: exit 3, one 0600 file exists, and stdout names the reason. The 401 is not retried (the mock expects 1 hit).
  - `over_limit_saves_and_fails`: with `max_items: 1` and 4 items, exit 3, nothing is posted, and the message contains "over this org's limit of 1".
  - `runner_present_409_exits_zero`.
  - `from_file_posts_saved_report`: a saved report is posted unchanged, the scan does not run (the fixture `HOME` is empty, yet the posted body has 4 items), and exit 0.
  - `token_never_in_argv_or_output`: the token value does not appear in stdout or stderr in any test.
- [ ] **Step 2: Run them to verify they fail.** Run `cargo test -p scenery-cli upload_`. Expected: FAIL.
- [ ] **Step 3: Implement it.** Build the client with `scenery_core::http::async_client_builder()` and a 30 s timeout, so the version header is added the same way as elsewhere.
- [ ] **Step 4: Run them to verify they pass.** Expected: PASS.
- [ ] **Step 5: Check against dev.** Run `./devbox.sh start` in scenery-backend, and create a token in the dev admin (or with `POST /admin/scan-tokens` via curl as the dev admin). Then run `HOME=$(mktemp -d) SCENERY_REGISTRY_URL=https://dev.scenery.dev SCENERY_SCAN_TOKEN=… target/debug/scenery scan --once` against a fixture `HOME`. Expected: exit 0, and `GET /admin/inventory` on dev lists the fixture items.
- [ ] **Step 6: Commit.** `git commit -m "scan: upload with org scan token, offline save, --from-file"`

### Task R5: `--login`: in-process browser sign-in, revoked at exit

**Files:**
- Create: `crates/scenery-cli/src/scan/login.rs`, `crates/scenery-cli/src/scan/login_tests.rs`
- Modify: `crates/scenery-cli/src/scan/mod.rs`, `crates/scenery-cli/src/main.rs` (make `try_open_browser` `pub(crate)`)

**Interfaces:**
- Consumes: `scenery_daemon::oauth_listener::start_listener`, `scenery_daemon::oauth_state::OAuthState` (`subscribe`, `cancel_all`); `scenery_core::auth::AuthClient` (`initiate_oauth_flow_with_redirect`, `exchange_oauth_code`, `revoke`). These are blocking, so call them in `spawn_blocking`.
- Produces: `login::sign_in(base: &str, open_browser: impl Fn(&str) -> bool) -> Result<LoginSession>`, where `LoginSession { access_token: String, refresh_token: String }` is held in memory only. The redirect is `http://127.0.0.1:{port}/oauth/cli/callback` on the listener's port (39127–39136). The wait is at most 5 minutes, after which it errors with "Sign-in timed out." The listener task is aborted before `sign_in` returns. `login::sign_out(base, &LoginSession)` calls `AuthClient::revoke(refresh_token, "family")` and is best-effort: a failure prints one warning line. `run` calls `sign_out` on every exit path after a successful `sign_in`, including upload failure.

- [ ] **Step 1: Write the failing tests** in `login_tests.rs`, with mockito for `/portal/auth/cli/token` and `/portal/auth/revoke`:
  - `login_happy_path_revokes_session`: the injected `open_browser` performs a `GET` to the callback URL with the `state` and `code` from the auth URL. `sign_in` returns the tokens. After `run` completes, the revoke mock was hit once with `scope: "family"`.
  - `login_revokes_even_when_upload_fails`: the upload mock returns 500, the exit is 3, and revoke is hit once.
  - `login_times_out`: with a 1 s test timeout (`pub(crate) const` overridable in tests), the error is "Sign-in timed out." and no listener port stays bound (rebinding 39127 succeeds).
  - `login_writes_no_tokens`: in the temp `HOME`, there is no state dir and no secret file afterwards. With `SCENERY_DISABLE_KEYCHAIN` unset, assert `scenery_core::auth::load_all_tokens` is never reached, by checking that the state dir does not exist.
- [ ] **Step 2: Run them to verify they fail.** Run `cargo test -p scenery-cli login_`. Expected: FAIL.
- [ ] **Step 3: Implement it.** Print the auth URL when `open_browser` returns false. Headless SSH users copy the URL, as with `auth login`.
- [ ] **Step 4: Run them to verify they pass.** Expected: PASS.
- [ ] **Step 5: Check against dev.** On the Mac with a fresh `HOME`, run `scenery scan --once --login`. Sign in as a dev test user, and confirm the inventory shows the user's email as the person. In the portal's session list, the CLI session is revoked.
- [ ] **Step 6: Commit.** `git commit -m "scan: --login with in-process browser sign-in, session revoked at exit"`

### Task R6: `scan.sh` one-liner, footprint test, release 2.8.0

**Files:**
- Create: `scripts/scan.sh`, `scripts/test_scan_sh.sh`, `crates/scenery-cli/tests/scan_footprint.rs`
- Modify: `Cargo.toml` (workspace version `2.8.0`), `CLAUDE.md` (one paragraph: "Runnerless scan (2.8.0)"), `/Users/shadowduck/code/scenery/scenery-backend/deploy.sh:131-145` (also copy `../scenery/scripts/scan.sh` to `install/scan.sh`), `/Users/shadowduck/code/scenery/scenery-backend/.gitignore` (`install/scan.sh`)

**Interfaces:**
- Consumes: the verification block from `scripts/install.sh` (lines 172–230: SHA256SUMS, minisign key `73F4BD707C872144`, `gh attestation` fallback).
- Produces: `scan.sh`, served at `https://install.scenery.dev/scan.sh`. It downloads the release tarball for this platform into `mktemp -d`, verifies it exactly as `install.sh` does (refusing on failure), and runs `scenery scan --once --login "$@"`, or with no `--login` when `SCENERY_SCAN_TOKEN` is set. It removes the temp dir in an `EXIT` trap and exits with the scan's code. It never writes outside its temp dir, and never touches `PATH`, shell rc files or `~/.local/bin`. `SCENERY_SCAN_VERSION` pins a tag (default: the latest release).

- [ ] **Step 1: Write the failing tests.**
  - `scripts/test_scan_sh.sh`: serve a fake release dir with `python3 -m http.server`, pointing `scan.sh` at it through `SCENERY_SCAN_BASE_URL` (test-only). Assert: a tampered tarball gives a non-zero exit and "refusing"; a good tarball whose fake `scenery` prints its argv gives `scan --once --login` without a token, and `scan --once` with `SCENERY_SCAN_TOKEN`; after either run, the temp dir is gone and `HOME` is unchanged.
  - `crates/scenery-cli/tests/scan_footprint.rs` `scan_leaves_no_footprint`: build a fixture `HOME`, snapshot it (path, size, mtime, mode), run the built binary with `--dry-run`, then with a mockito upload. The snapshot is equal after both runs, no `Scenery` data dir exists, and no child process of the test remains (`pgrep -P <pid>` is empty).
- [ ] **Step 2: Run them to verify they fail.** Run `sh scripts/test_scan_sh.sh; cargo test -p scenery-cli --test scan_footprint`. Expected: FAIL.
- [ ] **Step 3: Write `scan.sh`, and update deploy.sh and .gitignore in the backend.** Check on the dev box that Caddy's `install.*` site serves `/scan.sh` from `/srv/install` (the box Caddyfile is not in rsync). If it only rewrites `/` to `install.sh`, add a `file_server` for `/scan.sh` by hand on each box and record that in the backend deploy notes.
- [ ] **Step 4: Run them to verify they pass.** Then run the full suite: `cargo test && cargo clippy && cargo fmt --check`. Expected: PASS.
- [ ] **Step 5: Run the release gate on a release build.** Run `cargo build --release`, then `cargo test -p scenery-daemon --release --lib idle_cost -- --ignored --nocapture --test-threads=1` and `scripts/measure_daemon_rss.sh`. Expected: idle CPU under 0.6 s per 10 min, and RSS under 50 MB. Then run `/usr/bin/time -l target/release/scenery scan --once --dry-run` on the Mac. Record the peak RSS in the commit message (informational). Afterwards, `pgrep -f 'scenery scan'` is empty. Block the release on any failure.
- [ ] **Step 6: Commit, tag, release.** Run `git commit -m "Release 2.8.0: scenery scan --once (runnerless inventory)"`, then `git tag v2.8.0 && git push origin main v2.8.0`, then `SCENERY_MINISIGN_KEY=~/.ssh/scenery-release.key scripts/release.sh v2.8.0` in the owner's terminal, since it prompts for the minisign password. Before that, the backend from backend plan Tasks 1–8 must be live on prod (`./train.sh`), because older backends answer 404 on `/scan/*`. Afterwards, `brew upgrade scenery` on spaceghost and the Mac, then restart the daemons as the root CLAUDE.md says. Confirm with the acceptance-tester that the spaceghost daemon re-registers with `scan_identity` and still syncs.
