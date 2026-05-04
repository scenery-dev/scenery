# vectorhawkd

The VectorHawk runner — a per-user local agent that delivers governed AI tools and skills to AI clients (Claude Code, Cursor, VS Code, Gemini CLI, and others) over the Model Context Protocol.

> **Internal name:** `vectorhawkd`. **Public/marketing name:** "the VectorHawk runner" or just "the runner". Use the public name in user-facing docs, error messages, and marketing copy.

## Architecture

The runner is a **per-user daemon** with a **thin per-session stdio shim**:

```
AI client  --stdio JSON-RPC-->  vectorhawk mcp serve  (shim)
                                          |
                                          | Unix socket / named pipe
                                          v
                                    vectorhawkd  (daemon, LaunchAgent / systemd-user / Win service)
                                          |
                                          v
                          Backend MCP servers, registry, gateway, OAuth, audit
```

The daemon (`vectorhawkd`) holds all cross-session state: backend MCP connections, audit buffer, policy cache, OAuth callbacks, credential broker client, session budgets. The shim relays MCP traffic between the AI client and the daemon, with an in-process fallback if the daemon is unreachable.

See [`../context/RUN1_DAEMON_PIVOT_DECISION.md`](../context/RUN1_DAEMON_PIVOT_DECISION.md) and [`../context/RUN1_DAEMON_SPEC.md`](../context/RUN1_DAEMON_SPEC.md) for the design rationale.

## Crate layout

```
crates/
├── vectorhawkd-manifest/   library — skill bundle types, manifest parsing
├── vectorhawkd-core/       library — state, installer, registry client, policy, executor
├── vectorhawkd-mcp/        library — MCP protocol, aggregator, backend trait
├── vectorhawkd-daemon/     binary — vectorhawkd long-running agent
├── vectorhawkd-shim/       binary — stdio↔socket relay with in-process fallback
└── vectorhawkd-cli/        binary — vectorhawk user CLI (skill, mcp, daemon, doctor commands)
```

## Binaries

- `vectorhawkd` — the daemon. Started by LaunchAgent / systemd-user / Windows service. Listens on a Unix domain socket / named pipe. Not directly invoked by users.
- `vectorhawk` — the user-facing CLI. `vectorhawk mcp serve` is what AI clients spawn (it's the shim). `vectorhawk skill install`, `vectorhawk doctor`, etc. are user commands.

## Build

```bash
cargo build
cargo build --release
cargo test
cargo clippy
```

## Install

### Homebrew (macOS)

```bash
brew install vectorhawk/tap/vectorhawk
```

Supports macOS arm64 (Apple Silicon) and macOS x86_64 (Intel). The Homebrew formula
selects the correct binary for your architecture automatically.

### Linux

A curl-install script is available for Linux x86_64:

```bash
curl -fsSL https://install.vectorhawk.ai | sh
```

### Manual install

Download the tarball for your platform from the [GitHub Releases](../../releases) page, extract it,
and place the binaries on your `$PATH`.

## Supported targets

| Target | Platform |
|--------|----------|
| `aarch64-apple-darwin` | macOS arm64 (Apple Silicon) |
| `x86_64-apple-darwin` | macOS x86_64 (Intel) — cross-compiled from arm64 host |
| `x86_64-unknown-linux-gnu` | Linux x86_64 |

## Uninstalling

- **curl install**: `rm -rf ~/.vectorhawk` and remove the `$PATH` entry the installer added to your
  shell rc file (e.g. `~/.zshrc` or `~/.bashrc`).
- **Homebrew**: `brew uninstall vectorhawk`

Neither of these uninstall the LaunchAgent or systemd user unit. To remove those, run
`vectorhawk daemon uninstall` before removing the binaries.

## License

Apache-2.0. See [LICENSE](./LICENSE).
