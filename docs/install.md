# Installing the VectorHawk runner

## Quick install

```sh
curl -fsSL https://install.vectorhawk.ai | sh
```

This script detects your platform, downloads the matching pre-built binary from the latest
GitHub Release, verifies the SHA256 checksum, and installs the binaries to `~/.vectorhawk/bin/`.

Supported platforms:

- macOS arm64 (Apple Silicon)
- macOS x86_64 (Intel)
- Linux x86_64

## After install

The installer prints the next steps:

```
vectorhawk daemon install   # register the daemon as a login item (LaunchAgent on macOS, systemd-user on Linux)
vectorhawk mcp setup        # write the MCP entry to your AI client config
```

Neither step runs automatically. Both are intentional user actions.

## Override knobs

| Env var / flag               | Effect                                                                 |
|------------------------------|------------------------------------------------------------------------|
| `VECTORHAWK_VERSION=v0.1.0`  | Install a specific release tag instead of latest                       |
| `VECTORHAWK_HOME=<path>`     | Install binaries to `<path>/bin/` (default: `~/.vectorhawk/bin/`)     |
| `VECTORHAWK_NO_MODIFY_PATH=1` or `--no-modify-path` | Skip shell rc file modification entirely |
| `VECTORHAWK_VERBOSE=1` or `--verbose` | Print each download URL, hash comparison, and file move    |
| `--system`                   | Install to `/usr/local/bin/` if it is writable (no sudo required)     |

Examples:

```sh
# Install a specific version
curl -fsSL https://install.vectorhawk.ai | VECTORHAWK_VERSION=v0.2.0 sh

# Install to a custom directory, no PATH prompt
curl -fsSL https://install.vectorhawk.ai | VECTORHAWK_HOME=/opt/vectorhawk VECTORHAWK_NO_MODIFY_PATH=1 sh

# Verbose output to see what the script is doing
curl -fsSL https://install.vectorhawk.ai | VECTORHAWK_VERBOSE=1 sh
```

## Idempotency

Running the installer a second time when the same version is already present is a no-op:

```
vectorhawk 0.1.0 is already installed.
```

The script also guards against double-writing PATH entries in shell rc files.

## Uninstall

```sh
# Remove binaries and state directory
rm -rf ~/.vectorhawk

# Then remove the PATH line the installer added to your shell rc file.
# For zsh (check ~/.zshrc):
#   Remove the line:  export PATH="$HOME/.vectorhawk/bin:$PATH"
# For bash (check ~/.bashrc):
#   Remove the line:  export PATH="$HOME/.vectorhawk/bin:$PATH"
# For fish (check ~/.config/fish/config.fish):
#   Remove the line:  set -x PATH "$HOME/.vectorhawk/bin" $PATH
```

Note: uninstalling the binaries does not remove the LaunchAgent or systemd unit if you ran
`vectorhawk daemon install`. To remove those first, run `vectorhawk daemon uninstall` while the
binaries are still present.

For Homebrew installs: `brew uninstall vectorhawk`

## Homebrew (macOS)

A Homebrew tap is available as an alternative install path (D1.3):

```sh
brew tap vectorhawk/tap
brew install vectorhawk
```

The tap formula downloads the same release artifacts as the curl installer. `brew upgrade vectorhawk`
picks up new releases automatically.
