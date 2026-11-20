# macOS .pkg installer for MDM rollout — design

**Date:** 2026-11-20 · **Component:** runner · **Board card:** "Ship signed + notarized macOS .pkg installer for the runner (MDM)"

## Intent

IT pushes one `.pkg` through Jamf, Kandji or Intune. Every user who logs in on that Mac gets a running Scenery daemon and has Claude Code (and the other supported clients) wired to it, with no terminal and no setup by IT for each user. Curl and Homebrew stay the paths for individuals. This is the enterprise path.

## Goals

- A single `.pkg` that installs the runner system-wide and covers every user, including accounts created after install.
- Each employee signs in **once** (the existing OAuth/pairing flow). After that the device stays paired and they never sign in again.
- IT owns the install: it can upgrade the runner by pushing a newer `.pkg` and remove it with an uninstall script.
- The build pipeline produces the `.pkg` on every release. It is unsigned until the Apple Developer account exists, and signing and notarization switch on when the secrets are present.

## Non-goals

- Silent or zero-touch pairing (an org enrollment token). Deferred until a pilot customer asks for it.
- Pushing credentials, pairing codes or tokens through MDM profiles.
- A GUI or menu-bar app (DESKTOP1 is separate).
- Windows `.msi`.
- Revoking access on the device. Offboarding already works centrally: an admin revokes the user in the Scenery admin UI, which cascades to their devices, MCP access and provider OAuth.

## Anti-goals

- Any per-user state that IT can't remove or replace from the MDM side.
- Two daemons fighting over the single-instance socket lock (a brew agent or a `daemon install` agent running alongside the pkg agent).
- An upgrade that leaves the old binary running. This happened on spaceghost.
- A binary or LaunchAgent that the user can modify. These files are root-owned so the governance layer can't be swapped out.

## Constraints (from the existing code)

- Today's daemon is a per-user LaunchAgent: `scenery daemon install` writes `~/Library/LaunchAgents/com.scenery.agent.plist` (`crates/scenery-cli/src/install/macos.rs`), and `reconcile_competing_manager()` already handles a competing `homebrew.mxcl.scenery` agent.
- `mcp setup` writes per-user client configs. It must run as the user, not root.
- Pairing codes are per-user, expire after 10 minutes (`PAIR_CODE_TTL_MINUTES`, `scenery-backend/backend/app/routers/portal_devices.py`) and are single-use. `SCN_PAIR_CODE` therefore can't pair a fleet, and the .pkg does no pairing.
- The daemon does **not** run `mcp setup` today.
- Registry URL defaults to prod (`https://app.scenery.dev`), overridable with `SCENERY_REGISTRY_URL`.

## Approaches considered

1. **System-wide LaunchAgent that is the daemon — CHOSEN.** One root-owned plist in `/Library/LaunchAgents/`. launchd starts it in every user's GUI session, including new users. The daemon wires up clients itself when it starts. IT owns one file. The cost is that the daemon must stand down any per-user agent (brew or `daemon install`).
2. **Login-time bootstrap job.** A system agent runs a new `scenery bootstrap` command at each login, which writes the per-user plist and runs `mcp setup`. Rejected: it creates two launchd jobs for each user, and per-user plists end up spread across home folders where IT can't easily reach or remove them.
3. **Postinstall loops over users with `launchctl asuser`.** Rejected: fragile, misses users added later, and still needs 1 or 2 on top.

## Design

### Payload and layout

| Path | Owner/mode | What |
|---|---|---|
| `/Library/Application Support/Scenery/bin/scenery` | root:wheel 0755 | the universal binary (`lipo` of aarch64 + x86_64) |
| `/usr/local/bin/scenery`, `/usr/local/bin/scn` | root symlinks | created only if absent or already pointing at our binary; never overwrite a brew-owned file |
| `/Library/LaunchAgents/dev.scenery.agent.plist` | root:wheel 0644 | the system agent (below) |
| `/Library/Application Support/Scenery/uninstall.sh` | root:wheel 0755 | the uninstall script for IT |

- Package identifier is `dev.scenery.runner`. *(My call; the card proposed this.)*
- Install location is `/Library/Application Support/Scenery/` rather than `/usr/local/bin`, so it can't collide with Intel Homebrew and so the binary is root-owned. *(My call.)*

### The system agent

- It reuses the existing plist shape (`render_plist`): `daemon run --foreground`, `RunAtLoad`, `KeepAlive`, `ProcessType=Background`, `Umask 63`, and `LimitLoadToSessionType=Aqua` so it runs only in GUI login sessions.
- The label is `dev.scenery.agent`, distinct from the per-user `com.scenery.agent`, so launchd never confuses the two.
- Logs go to the user's `~/Library/Logs/Scenery/`, the same place as today.
- No registry URL is baked in. It defaults to prod. **v1 ships** reading an optional `SCENERY_REGISTRY_URL` from the managed-preferences domain `dev.scenery.runner`, so IT, and we on dev, can point the agent elsewhere without a different pkg. *(My call.)*
- The agent's `PATH` includes `~/.local/bin`, so `mcp setup` finds `claude` and writes the HTTP entry, not the stdio fallback.

### Daemon changes (runner code)

1. **Detect pkg-managed mode.** The daemon is pkg-managed when its executable path is under `/Library/Application Support/Scenery/` (or the system plist exists).
2. **Stand down competing agents.** In pkg-managed mode, the daemon boots out and disables any per-user `com.scenery.agent` and `homebrew.mxcl.scenery` agent at startup, extending `reconcile_competing_manager()`, so it holds the lock alone. It leaves files in the user's home untouched apart from that.
3. **`scenery daemon install` refuses** when the system agent is present, with a clear message saying IT manages Scenery on this Mac.
4. **Wire up clients at startup.** The daemon runs `mcp setup` idempotently each time it starts, for every client found. **Re-adding:** in pkg-managed mode the Scenery entry is restored on every start even if the user deleted it, because IT is enforcing it there. In brew/curl installs it is added only the first time per client, and a deliberate removal is remembered and respected. *(Decided by Adam 2026-11-20.)* It re-checks when a client's config file appears later, for example when Claude Code is installed after Scenery. This applies to all install methods, not only the pkg, and fixes the case where a client is installed after the runner.
5. **Existing brew/curl installs.** In pkg-managed mode, startup rewrites any client entry that points at another `scenery` binary (brew, `~/.local/bin`) to the pkg binary, so shim and daemon are always the same version. The `/usr/local/bin` symlinks are skipped if brew owns that path, and the brew binary is left installed but unused. *(My call.)*
6. **More than one session.** Designed and tested for one active GUI user. With fast user switching, each session gets its own daemon on its own port (39140-39149 already allows this), and the postinstall loop handles several users. *(My call.)*
7. **First sign-in** uses the existing flow and nothing new. Claude Code shows the Scenery MCP as needing Authenticate (HTTP + OAuth entry), and `scenery auth pair` works too. *(My call: no new pop-up or notification in v1.)*

### Install and upgrade scripts

- **preinstall:** nothing beyond what's needed (bootout any loaded `dev.scenery.agent` for logged-in users so the binary swap is clean).
- **postinstall:** create the symlinks, then for each user with a GUI session run `launchctl bootstrap gui/<uid>` on the plist, or `kickstart -k` if it's already loaded. This makes an upgrade replace the running binary right away instead of waiting for the next login.

### Uninstall (`uninstall.sh`, run as root by IT/MDM)

- For each logged-in user: bootout `gui/<uid>/dev.scenery.agent`, then run a **new narrow mode**, `scenery uninstall --local-only`, as that user (`launchctl asuser`). It restores client configs and removes Scenery-placed skill folders from clients, but makes **no server calls**: no archiving of the user's private skills and no uninstall outbox. Offboarding stays central in the admin UI, so removing the pkg is safe for a reinstall or a laptop handed to someone else. *(Decided by Adam 2026-11-20.)*
- Remove the plist, binary, symlinks, then `pkgutil --forget dev.scenery.runner`.
- *(My call: users who aren't logged in keep stale client entries pointing at a missing binary. This is acceptable for v1, and the entries do nothing.)*
- Per-user state (`~/Library/Application Support/Scenery/`) is left in place.

### Build and release

- A new `pkg` job in `release.yml` runs on macOS after both mac targets build. It does `lipo`, stages the payload, runs `pkgbuild` (component pkg with scripts), and then `productbuild` (distribution pkg). The output is `scenery-<version>-macos.pkg`, and its hash goes into `SHA256SUMS`.
- The pkg is built and notarized in CI **before** the draft release, so `SHA256SUMS` already holds the final pkg hash and `scripts/release.sh`'s minisign step runs unchanged afterwards. Only the binary inside the pkg gets a Developer ID signature; tarballs and the brew formula are unchanged. *(My call.)*
- **Signing is gated on secrets.** If `APPLE_DEVELOPER_ID_*` secrets are present, it signs the binary (Developer ID Application, hardened runtime), signs the pkg (Developer ID Installer), runs `notarytool submit --wait`, then `stapler staple`. If the secrets are absent, it ships unsigned and logs that it did.
- A local `scripts/build-pkg.sh` does the same steps so the pkg can be built and tested on the Mac without CI.

### Testing (unsigned)

- Install with `sudo installer -pkg … -target /`. This bypasses Gatekeeper for unsigned packages.
- Run it on this Mac and on a fresh macOS VM with two user accounts, and check:
  - the agent starts for each user
  - clients get wired up
  - one-time sign-in persists across a reboot
  - an upgrade swaps the running binary
  - `uninstall.sh` restores client configs
  - a pre-existing brew install gets stood down
- Rust unit tests cover pkg-managed detection, stand-down decisions and the `daemon install` refusal.

## Left to the builder

- Exact `pkgbuild`/`productbuild` flags, the distribution XML, and the minimum macOS version.
- How the daemon watches for clients that are installed later (polling vs FSEvents).
- Postinstall details for finding GUI-session users (`scutil`/`who`/`launchctl print`).
- Managed preferences scope: only the device-level managed prefs path (`/Library/Managed Preferences/dev.scenery.runner`) is read; user-scoped profiles are not. MDM profiles must target the device.
- Portal OnboardingGate download link for the `.pkg`. This is a follow-up once it's signed.

## Blocked on the user (not needed to start)

- Apple Developer Program enrollment for Scenery Technologies LLC, Developer ID Application and Installer certs, an App Store Connect API key, and the GitHub secrets for all of them.
