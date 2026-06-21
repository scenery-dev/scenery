# CLAUDE.md — vectorhawkd

This file provides guidance to Claude Code (claude.ai/code) when working in this repo.

## Task Tracking — read this to understand project state

**All tasks are tracked on the GitHub Project board, not in this file.** It is the single source of truth for what's planned, in flight, and done across every VectorHawk repo.

- **Board:** VectorHawk Roadmap → https://github.com/orgs/vectorhawk/projects/1 (org-level, private)
- **Status flow:** Backlog → Todo → In Progress → Blocked → Done
- **Component field:** `runner` · `backend` · `gateway` · `portal` · `harness` · `site` — runner work in this repo uses `runner`.

At the start of a session, list open tasks to orient yourself; when you pick up or finish work, move the card. Do not maintain ad-hoc TODO lists in code or docs — file a card instead.

```bash
# Runner tasks, current state
gh project item-list 1 --owner vectorhawk

# File a new task
gh project item-create 1 --owner vectorhawk --title "..." --body "..."

# Open the board
gh project view 1 --owner vectorhawk --web
```

## What this is

`vectorhawkd` is the **VectorHawk runner** — a per-user local Rust agent that delivers governed AI tools and skills to AI clients over MCP. It is the successor to `../skillrunner/` (now archived). The architecture pivot from per-AI-client stdio child process to long-running daemon + thin shim is documented in:

- `../context/RUN1_DAEMON_PIVOT_DECISION.md`
- `../context/RUN1_DAEMON_SPEC.md`
- `../context/IMPLEMENTATION_PLAN_v3.md` (Phase RUN1)

## Naming

- Internal / code / repo: `vectorhawkd`
- Public / marketing / user-facing docs: "the VectorHawk runner" or "the runner"
- Single shipped binary: `vectorhawk` (from `vectorhawkd-cli`) — what users actually type.
  - `vectorhawk daemon run` starts the long-running daemon (previously a standalone `vectorhawkd` binary).
  - `vectorhawk mcp serve` is the MCP relay/shim (previously a standalone `vectorhawkd-shim` binary).
  - `vectorhawkd` is the repo/workspace name only, not a binary name.

## Architecture (M0 target)

```
AI client  --stdio MCP JSON-RPC-->  vectorhawk mcp serve  (shim, ~5 MB RSS)
                                              |
                                              | Unix socket / named pipe
                                              v
                                        vectorhawkd  (daemon, ~40 MB RSS idle)
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
├── vectorhawkd-manifest/   library — manifest types, ports skillrunner-manifest
├── vectorhawkd-core/       library — state, installer, registry, policy, executor (ports skillrunner-core)
├── vectorhawkd-mcp/        library — MCP protocol, aggregator, Backend trait (ports skillrunner-mcp protocol/tools/aggregator/sampling/setup)
├── vectorhawkd-daemon/     library — daemon entry point (vectorhawkd_daemon::run_daemon); reused by vectorhawk CLI via `daemon run`
├── vectorhawkd-shim/       library — stdio↔socket relay entry point (vectorhawkd_shim::run_shim); reused by vectorhawk CLI via `mcp serve`
└── vectorhawkd-cli/        binary — vectorhawk (the only shipped binary; embeds daemon + shim via `daemon run` / `mcp serve` subcommands)
```

## Build & dev

```bash
cargo build
cargo build --release
cargo test
cargo test -p vectorhawkd-core
cargo clippy
cargo fmt
```

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

## Source for porting

Existing code at `../skillrunner/crates/`:
- `skillrunner-manifest/` → `vectorhawkd-manifest/` (rename only)
- `skillrunner-core/` → `vectorhawkd-core/` (rename only; SQLite paths change `SkillClub/SkillRunner/` → `VectorHawk/`)
- `skillrunner-mcp/protocol.rs`, `tools.rs`, `aggregator.rs`, `sampling.rs`, `setup.rs` → `vectorhawkd-mcp/`
- `skillrunner-mcp/server.rs` → split: dispatch loop into `vectorhawkd-mcp` as a `Server<B: Backend>` generic; daemon uses `SocketBackend`-equivalent + real backend; shim uses `SocketBackend` (relay) with `EmbeddedBackend` fallback
- `skillrunner-cli/` → `vectorhawkd-cli/` (rename + new `mcp serve` becomes the shim, new `daemon run/install/uninstall` subcommands)

## What is NOT in M0 scope

- Full feature parity with skillrunner. The spike proves the architecture works.
- Windows support (deferred to M2/M3).
- LaunchAgent/systemd unit packaging (deferred to M2).
- OAuth callback listener (deferred to M3).
- Audit upload, registry sync, update check (M1).
- `mcp setup` writing real configs (M1; M0 just validates the wire format hasn't changed).
- Production-quality error handling. Spike-grade is fine.

## What MUST be in M0

1. Workspace builds (`cargo build` green).
2. `vectorhawkd` daemon boots, listens on a Unix socket at `~/Library/Application Support/VectorHawk/agent.sock` (mac) / `$XDG_RUNTIME_DIR/vectorhawk/agent.sock` (Linux), holds at least one stub backend MCP connection.
3. `vectorhawk mcp serve` (shim) connects to the socket, relays an MCP `initialize` handshake + `tools/list` + ≥5 `tools/call` invocations from a real AI client (Claude Code preferred).
4. Killing the daemon mid-session causes the shim to surface a JSON-RPC error (code -32001) containing the `vectorhawk daemon install` hint within 3 seconds. (M0 originally accepted silent in-process fallback; M4 changed the contract — see `context/RUN1_M4_STREAMS.md`.)
5. Daemon idle RSS measured ≤50 MB on macOS arm64. Linux measurement deferred-but-not-blocking.
6. `mcp setup` (or equivalent) demonstrates that the AI client config entry would be `command = "vectorhawk", args = ["mcp", "serve"]` — same shape as skillrunner today.
