# Scenery runner

The Scenery runner — a per-user local agent that delivers governed AI tools and skills to AI clients (Claude Code, Cursor, VS Code, Gemini CLI, and others) over the Model Context Protocol.

> **Name:** `scenery` (binary, crates, repo). Short alias: `scn`. In user-facing docs, error messages, and marketing copy, say "the Scenery runner" or just "the runner".

## Architecture

The runner is a **per-user daemon** with a **thin per-session stdio shim**:

```
AI client  --stdio JSON-RPC-->  scenery mcp serve  (shim)
                                          |
                                          | Unix socket / named pipe
                                          v
                                    scenery daemon  (LaunchAgent / systemd-user)
                                          |
                                          v
                          Backend MCP servers, registry, gateway, OAuth, audit
```

The daemon (`scenery daemon run`) holds all cross-session state: backend MCP connections, audit buffer, policy cache, OAuth callbacks, credential broker client, session budgets. The shim relays MCP traffic between the AI client and the daemon, with an in-process fallback if the daemon is unreachable.

See [`../context/RUN1_DAEMON_PIVOT_DECISION.md`](../context/RUN1_DAEMON_PIVOT_DECISION.md) and [`../context/RUN1_DAEMON_SPEC.md`](../context/RUN1_DAEMON_SPEC.md) for the design rationale.

## Crate layout

```
crates/
├── scenery-manifest/   library — skill bundle types, manifest parsing
├── scenery-core/       library — state, installer, registry client, policy, executor
├── scenery-mcp/        library — MCP protocol, aggregator, backend trait
├── scenery-daemon/     library — long-running agent entry point (`scenery daemon run`)
├── scenery-shim/       library — stdio↔socket relay entry point (`scenery mcp serve`)
└── scenery-cli/        binary — `scenery` (and `scn`), the only shipped binary
```

## Binaries

- `scenery` — the single shipped binary and user-facing CLI. `scenery daemon run` is the long-running daemon (started by LaunchAgent / systemd-user; users don't invoke it directly). `scenery mcp serve` is what AI clients spawn (the shim). `scenery skill install`, `scenery doctor`, etc. are user commands.
- `scn` — a short alias for `scenery` (a symlink next to the binary in every install path: Homebrew, `install.sh`, release tarball). `scn --help` and `scenery --help` behave identically.

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
scenery skill init my-skill

# Edit the scaffold, then validate
scenery skill validate my-skill/

# Publish to the registry
scenery skill publish my-skill/ --registry-url https://app.scenery.dev
```

See [`docs/AUTHORING.md`](docs/AUTHORING.md) for the full authoring guide and
[`crates/scenery-manifest/schemas/skill_md_frontmatter.json`](crates/scenery-manifest/schemas/skill_md_frontmatter.json)
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
brew tap scenery-dev/tap
brew trust scenery-dev/tap
brew install scenery-dev/tap/scenery
brew services start scenery
scenery mcp setup
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
curl -fsSL https://install.scenery.dev | sh
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

Each release attaches per-target tarballs named `scenery-<version>-<triple>.tar.gz`, each containing
`scenery`, `scn` (a symlink to `scenery`), `LICENSE`, and `README.md`. A SHA256 checksum file
(`scenery-<version>-<triple>.tar.gz.sha256`) is attached alongside each tarball.

Pre-release tags (e.g. `v1.0.1-rc.0`) are marked as pre-releases on GitHub. Stable tags produce
full releases.

The curl-install script and Homebrew tap consume these release artifacts. The tarball name
and SHA256 format are the stable interface those scripts rely on.

## Uninstalling

- **curl install**: `rm -f ~/.local/bin/scenery ~/.local/bin/scn` and remove the `$PATH` entry the installer added to your
  shell rc file (e.g. `~/.zshrc` or `~/.bashrc`).
- **Homebrew**: `brew uninstall scenery`

Neither of these uninstall the LaunchAgent or systemd user unit. To remove those, run
`scenery daemon uninstall` before removing the binaries.

## License

Apache-2.0. See [LICENSE](./LICENSE).
