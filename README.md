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

## Authoring Skills

Skills are Markdown files with YAML frontmatter (`SKILL.md`). The body becomes the system prompt; frontmatter controls metadata, permissions, and execution settings.

```bash
# Scaffold a new skill
vectorhawk skill init my-skill

# Edit the scaffold, then validate
vectorhawk skill validate my-skill/

# Publish to the registry
vectorhawk skill publish my-skill/ --registry-url https://app.vectorhawk.ai
```

See [`docs/AUTHORING.md`](docs/AUTHORING.md) for the full authoring guide and
[`crates/vectorhawkd-manifest/schemas/skill_md_frontmatter.json`](crates/vectorhawkd-manifest/schemas/skill_md_frontmatter.json)
for the JSON Schema.

Example skills are in [`examples/skills/`](examples/skills/):
- `minimal/` — bare minimum (name + description + prompt body)
- `medium/` — permissions, execution limits, triggers
- `complex/` — multi-step workflow with external prompt files
- `contract-compare/` — production-grade skill with schemas

## Status

v1.0.0. See [`../context/IMPLEMENTATION_PLAN_v3.md`](../context/IMPLEMENTATION_PLAN_v3.md) for the full milestone history.

## Install

### Homebrew (macOS)

```bash
brew tap vectorhawk/tap
brew trust vectorhawk/tap
brew install vectorhawk
brew services start vectorhawk
vectorhawk mcp setup
```

`brew trust` is required by Homebrew 7+ for any third-party tap; without it
`brew install` refuses to load the formula. `brew services start` and
`mcp setup` are separate steps because Homebrew's install sandbox cannot write
to your home directory during `brew install`.

Supports macOS arm64 (Apple Silicon) and macOS x86_64 (Intel). The Homebrew formula
selects the correct binary for your architecture automatically.

### Linux

A curl-install script is available for Linux x86_64:

```bash
curl -fsSL https://install.vectorhawk.ai | sh
```

This script requires EC2 distribution infrastructure to be live before it is broadly advertised.
Check the release notes for current availability.

### Manual install

Download the tarball for your platform from the [GitHub Releases](../../releases) page, extract it,
and place the binaries on your `$PATH`.

## Supported targets

| Target | Platform |
|--------|----------|
| `aarch64-apple-darwin` | macOS arm64 (Apple Silicon) |
| `x86_64-apple-darwin` | macOS x86_64 (Intel) — cross-compiled from arm64 host |
| `x86_64-unknown-linux-gnu` | Linux x86_64 |

## Releasing

Releases are cut by pushing a semver git tag. GitHub Actions builds release binaries for all three
targets and attaches them to the matching GitHub Release automatically.

To cut a release:

```bash
git tag v1.0.1
git push origin v1.0.1
```

Each release attaches per-target tarballs named `vectorhawk-<version>-<triple>.tar.gz`, each containing
`vectorhawk`, `vectorhawkd`, `vectorhawkd-shim`, `LICENSE`, and `README.md`. A SHA256 checksum file
(`vectorhawk-<version>-<triple>.tar.gz.sha256`) is attached alongside each tarball.

Pre-release tags (e.g. `v1.0.1-rc.0`) are marked as pre-releases on GitHub. Stable tags produce
full releases.

The curl-install script and Homebrew tap consume these release artifacts. The tarball name
and SHA256 format are the stable interface those scripts rely on.

## Uninstalling

- **curl install**: `rm -rf ~/.vectorhawk` and remove the `$PATH` entry the installer added to your
  shell rc file (e.g. `~/.zshrc` or `~/.bashrc`).
- **Homebrew**: `brew uninstall vectorhawk`

Neither of these uninstall the LaunchAgent or systemd user unit. To remove those, run
`vectorhawk daemon uninstall` before removing the binaries.

## License

Apache-2.0. See [LICENSE](./LICENSE).
