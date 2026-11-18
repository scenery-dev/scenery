# CLAUDE.md — scenery (runner)

This file provides guidance to Claude Code (claude.ai/code) when working in this repo.

## Task Tracking — read this to understand project state

**All tasks are tracked on the GitHub Project board, not in this file.** It is the single source of truth for what's planned, in flight, and done across every Scenery repo.

- **Board:** Scenery Roadmap → https://github.com/orgs/scenery-dev/projects/1 (org-level, private)
- **Status flow:** Backlog → Todo → In Progress → Blocked → Done
- **Component field:** `runner` · `backend` · `gateway` · `portal` · `harness` · `site` — runner work in this repo uses `runner`.

At the start of a session, list open tasks to orient yourself; when you pick up or finish work, move the card. Do not maintain ad-hoc TODO lists in code or docs — file a card instead.

```bash
# Runner tasks, current state
gh project item-list 1 --owner scenery-dev

# File a new task
gh project item-create 1 --owner scenery-dev --title "..." --body "..."

# Open the board
gh project view 1 --owner scenery-dev --web
```

## What this is

`scenery` is the **Scenery runner** — a per-user local Rust agent that delivers governed AI tools and skills to AI clients over MCP. The architecture (long-running daemon + thin per-client shim) is documented in:

- `../context/RUN1_DAEMON_PIVOT_DECISION.md`
- `../context/RUN1_DAEMON_SPEC.md`
- `../context/IMPLEMENTATION_PLAN_v3.md` (Phase RUN1)

## Naming

- Repo / workspace / crates: `scenery` (`scenery-{manifest,core,mcp,daemon,shim,cli}`).
- Public / marketing / user-facing docs: "the Scenery runner" or "the runner".
- Single shipped binary: `scenery` (from `scenery-cli`) — what users actually type. `scn` is a short alias: a symlink to `scenery` placed next to it by every install path (release tarball, `scripts/install.sh`, the Homebrew formula's `bin.install_symlink`). The binary resolves symlinks via `current_exe`, so `scn` behaves identically.
  - `scenery daemon run` starts the long-running daemon.
  - `scenery mcp serve` is the MCP relay/shim.

## Architecture (M0 target)

```
AI client  --stdio MCP JSON-RPC-->  scenery mcp serve  (shim, ~5 MB RSS)
                                              |
                                              | Unix socket / named pipe
                                              v
                                        scenery  (daemon, ~40 MB RSS idle)
                                              |
                                  ┌───────────┼────────────┐
                                  v           v            v
                        Backend MCP    Registry HTTPS    OAuth callback
                        (HTTP/2)       (audit, policy)   (fixed local port)
```

- **Daemon** owns: BackendRegistry (live backend MCP connections), SQLite, audit buffer, policy cache, registry sync, OAuth callbacks, credential broker client, future session budgets / kill switch.
- **Shim** owns: one stdio connection to AI client, one socket to daemon, JSON-RPC frame relay, in-process fallback if daemon unreachable >2s.

## Crate layout

```
crates/
├── scenery-manifest/   library — manifest types
├── scenery-core/       library — state, installer, registry, policy, executor
├── scenery-mcp/        library — MCP protocol, aggregator, Backend trait, client setup
├── scenery-daemon/     library — daemon entry point (scenery_daemon::run_daemon); reused by the CLI via `daemon run`
├── scenery-shim/       library — stdio↔socket relay entry point (scenery_shim::run_shim); reused by the CLI via `mcp serve`
└── scenery-cli/        binary — scenery (the only shipped binary; embeds daemon + shim via `daemon run` / `mcp serve`)
```

## Build & dev

```bash
cargo build
cargo build --release
cargo test
cargo test -p scenery-core
cargo clippy
cargo fmt
```

## Testing unreleased builds

`scripts/dev-push.sh [host] [--status|--revert]` builds the working tree on
spaceghost (remote cargo, no tag/signing/brew), installs it to
`~/.local/share/Scenery/dev-bin/`, and activates it via a second systemd
drop-in (`zz-dev-binary.conf`; `local.conf` untouched). Use `scenery-dev` for the
dev CLI. The override survives `brew services restart` until `--revert`. On macOS:
`brew services stop scenery`, `cargo build --release`, then run
`target/release/scenery daemon run --foreground`.

## Authoring with Scenery (2.4.0)

Local edits (Phase 9a, 2.5.0): `managed_paths/drift.rs` hashes a skill's WHOLE tree (`scenery_core::skill_tree`, marker `hash_version=2`; a v1 marker is re-baselined once if `SKILL.md` still matches, else it is an edit). `DriftStatus` for skills is `Clean | EditedOwn | EditedCatalog | Deleted` (MCP/plugins keep `Drifted`); authorship comes from the snapshot (`skill_local_state`, `managed_paths/local_edits.rs`), falling back to the auto-managed marker. `local_edits::upload_edit` sends the edited tree to `POST /runner/skills/{id}/versions/local` (own = private version, catalog = branch), debounced (an edit with a file modified in the last `SCENERY_LOCAL_EDIT_SETTLE_SECS`, default 30, waits for the next scan), holds a tree with a secret pattern, retries failures, then makes the tree the marker baseline and reports the branch state (`PATCH /api/installations/{id}`). Own skills always run, in every mode; catalog edits are branches in `audit_only`/`warn` and uploaded-then-quarantined in the strict modes.

MCP tools `scenery_create_skill`, `scenery_check_skill`, `scenery_share_skill` (exposed by default, as are `scenery_mcp_catalog` / `scenery_mcp_request` when signed in; the other management tools need `SCENERY_EXPOSE_BUILTIN_TOOLS=1`) live in `crates/scenery-mcp/src/authoring_tools.rs`; the daemon registers the create hook (`managed_paths/authoring.rs`: register, then the same transactional conversion as auto-manage). The built-in skill `authoring-with-scenery` is `crates/scenery-daemon/assets/authoring-with-scenery/SKILL.md`, written by `managed_paths/builtin.rs` at start (marker row `origin='builtin'`); discovery, auto-manage, drift, upload and Share skip it and `scenery uninstall` removes it. Share always submits with `force_review` (the confirm token goes to the AI, and the hand-rolled MCP server has no server-to-client request path for elicitation), so an AI-client share never publishes straight to the catalog. `path` arguments must resolve (canonicalized, symlinks followed) to a folder directly under `~/.agents/skills` or `~/.claude/skills`. Backend gate: the new endpoints answer 426 below runner 2.4.0.

## Releasing

After bumping the workspace `Cargo.toml` version, tag `vX.Y.Z` and push the tag
(CI builds a DRAFT release). Then, in your own terminal (it prompts for the
minisign password), run one command:

```bash
SCENERY_MINISIGN_KEY=~/.ssh/scenery-release.key scripts/release.sh vX.Y.Z
```

It preflights (tag on origin, version match, homebrew-tap and scenery-backend
clean on main), waits for the release workflow, runs `scripts/sign-release.sh
--apply-formula` (skipped if already signed), verifies/commits/pushes the
homebrew-tap formula (`scenery X.Y.Z`; refuses weekdays 9am-5pm ET unless
`--force-hours`), and stages the runner on the demo box. Resume a failed run
with `--from <preflight|wait|sign|formula|stage>`. Afterwards `brew upgrade
scenery` on spaceghost (see the root CLAUDE.md). Test: `scripts/test_release.sh`.

## Things to remember (per project conventions)

Before writing code:
1. State how you'll verify the change works (test, bash command, etc).
2. Write the verification first.
3. Implement.
4. Run verification, iterate until green.
5. `cargo clippy` and `cargo fmt` before committing.
6. Commit at each milestone with a clear message.

## Compute budget (binding for M0)

- Daemon idle: <50 MB RSS (target 35–45), <0.1% CPU
- Daemon under load: <100 MB RSS
- Shim: <8 MB RSS, <30 ms cold start, ~2–3 MB stripped binary

## What is NOT in M0 scope

- Windows support (deferred to M2/M3).
- LaunchAgent/systemd unit packaging (deferred to M2).
- OAuth callback listener (deferred to M3).
- Audit upload, registry sync, update check (M1).
- `mcp setup` writing real configs (M1; M0 just validates the wire format hasn't changed).
- Production-quality error handling. Spike-grade is fine.

## What MUST be in M0

1. Workspace builds (`cargo build` green).
2. `scenery` daemon boots, listens on a Unix socket at `~/Library/Application Support/Scenery/agent.sock` (mac) / `$XDG_RUNTIME_DIR/scenery/agent.sock` (Linux), holds at least one stub backend MCP connection.
3. `scenery mcp serve` (shim) connects to the socket, relays an MCP `initialize` handshake + `tools/list` + ≥5 `tools/call` invocations from a real AI client (Claude Code preferred).
4. Killing the daemon mid-session causes the shim to surface a JSON-RPC error (code -32001) containing the `scenery daemon install` hint within 3 seconds. (M0 originally accepted silent in-process fallback; M4 changed the contract — see `context/RUN1_M4_STREAMS.md`.)
5. Daemon idle RSS measured ≤50 MB on macOS arm64. Linux measurement deferred-but-not-blocking.
6. `mcp setup` (or equivalent) demonstrates that the AI client config entry would be `command = "scenery", args = ["mcp", "serve"]`.
