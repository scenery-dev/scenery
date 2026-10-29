# Installing the Scenery runner

## Quick install

```sh
curl -fsSL https://install.scenery.dev | sh
```

This script detects your platform, downloads the matching pre-built binary from the latest
GitHub Release, verifies the SHA256 checksum, and installs `scenery` and its short alias `scn` (a symlink) to `~/.local/bin/`.

Supported platforms:

- macOS arm64 (Apple Silicon)
- macOS x86_64 (Intel)
- Linux x86_64

## After install

The installer prints the next steps:

```
scenery daemon install   # register the daemon as a login item (LaunchAgent on macOS, systemd-user on Linux)
scenery mcp setup        # write the MCP entry to your AI client config
```

Neither step runs automatically. Both are intentional user actions.

## Override knobs

| Env var / flag               | Effect                                                                 |
|------------------------------|------------------------------------------------------------------------|
| `SCENERY_VERSION=v0.1.0`  | Install a specific release tag instead of latest                       |
| `SCENERY_NO_MODIFY_PATH=1` or `--no-modify-path` | Skip shell rc file modification entirely |
| `SCENERY_VERBOSE=1` or `--verbose` | Print each download URL, hash comparison, and file move    |
| `--system`                   | Install to `/usr/local/bin/` if it is writable (no sudo required)     |

Examples:

```sh
# Install a specific version
curl -fsSL https://install.scenery.dev | SCENERY_VERSION=v0.2.0 sh

# No PATH prompt
curl -fsSL https://install.scenery.dev | SCENERY_NO_MODIFY_PATH=1 sh

# Verbose output to see what the script is doing
curl -fsSL https://install.scenery.dev | SCENERY_VERBOSE=1 sh
```

## Idempotency

Running the installer a second time when the same version is already present is a no-op:

```
scenery 0.1.0 is already installed.
```

The script also guards against double-writing PATH entries in shell rc files.

## Uninstall

```sh
# Remove the binaries
rm -f ~/.local/bin/scenery ~/.local/bin/scn

# Then remove the PATH line the installer added to your shell rc file.
# For zsh (check ~/.zshrc):
#   Remove the line:  export PATH="$HOME/.local/bin:$PATH"
# For bash (check ~/.bashrc):
#   Remove the line:  export PATH="$HOME/.local/bin:$PATH"
# For fish (check ~/.config/fish/config.fish):
#   Remove the line:  fish_add_path "$HOME/.local/bin"
```

Note: uninstalling the binaries does not remove the LaunchAgent or systemd unit if you ran
`scenery daemon install`. To remove those first, run `scenery daemon uninstall` while the
binaries are still present.

For Homebrew installs: `brew uninstall scenery`

## Homebrew (macOS)

A Homebrew tap is available as an alternative install path (D1.3):

```sh
brew tap scenery-dev/tap
brew trust scenery-dev/tap
brew install scenery-dev/tap/scenery
brew services start scenery
scenery mcp setup
```

`brew trust` is required by Homebrew 7 and later before it will load a formula from a
third-party tap; without it `brew install` fails with "Refusing to load formula ... from
untrusted tap".

`brew services start` and `scenery mcp setup` are separate, manual steps. Homebrew runs
`post_install` inside a sandbox that denies writes to the user's real home directory (macOS
seatbelt, Linux Landlock), so the LaunchAgent/unit and the AI-client config cannot be written
during `brew install`.

The tap formula downloads the same release artifacts as the curl installer. `brew upgrade
scenery` picks up new releases automatically; the running agent notices its binary changed
and restarts itself onto the new version.
