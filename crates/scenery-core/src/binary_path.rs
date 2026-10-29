//! Resolve the stable, unversioned path to the running `scenery` binary.
//!
//! Shared by two callers that both need it for the same underlying reason —
//! a Homebrew Cellar install is versioned on disk but must be referenced by
//! a path that survives an upgrade:
//! - `scenery-cli`'s `install` module writes this path into the
//!   auto-start unit (`ExecStart=`/`ProgramArguments`) so `brew upgrade`
//!   relinking the symlink is picked up without re-running `daemon install`.
//! - `scenery-daemon`'s binary-replacement watch stats this same path
//!   periodically to detect that very relink and exit so the service
//!   manager restarts it — see `scenery_daemon::binary_watch`.
//!
//! Originally lived only in the CLI crate; moved here so the daemon crate
//! can use it too without duplicating the logic or depending on the CLI
//! crate.

/// `scn` is a symlink alias for `scenery`. `std::env::current_exe()` does not
/// resolve it on macOS, so a process started as `scn` would otherwise write
/// the alias path into service units and client configs. Map a trailing `scn`
/// component back to the sibling `scenery` binary; anything else is unchanged.
pub fn unalias_scn(exe: &std::path::Path) -> std::path::PathBuf {
    if exe.file_name() == Some(std::ffi::OsStr::new("scn")) {
        exe.with_file_name("scenery")
    } else {
        exe.to_path_buf()
    }
}

/// Rewrite a Homebrew Cellar path (`<prefix>/Cellar/<formula>/<version>/bin/<name>`)
/// to the unversioned symlink (`<prefix>/bin/<name>`). Any non-Cellar path is
/// returned unchanged. Extracted so unit tests can hit it without spawning a
/// real exe.
pub fn rewrite_homebrew_cellar_to_symlink(exe: &std::path::Path) -> std::path::PathBuf {
    let exe = unalias_scn(exe);
    let exe = exe.as_path();
    let Some(bin_name) = exe.file_name() else {
        return exe.to_path_buf();
    };

    let components: Vec<_> = exe.components().collect();
    for (idx, component) in components.iter().enumerate() {
        // OsStr literal comparison via `==` works because OsStr implements
        // PartialEq<str>.
        if component.as_os_str() == std::ffi::OsStr::new("Cellar") && idx >= 1 {
            let prefix: std::path::PathBuf = components[..idx].iter().collect();
            return prefix.join("bin").join(bin_name);
        }
    }

    exe.to_path_buf()
}

#[cfg(test)]
mod tests {
    #[test]
    fn unalias_scn_maps_alias_to_scenery() {
        use std::path::Path;
        assert_eq!(
            super::unalias_scn(Path::new("/opt/homebrew/bin/scn")),
            Path::new("/opt/homebrew/bin/scenery")
        );
        assert_eq!(
            super::unalias_scn(Path::new("/opt/homebrew/bin/scenery")),
            Path::new("/opt/homebrew/bin/scenery")
        );
    }

    use super::rewrite_homebrew_cellar_to_symlink;
    use std::path::Path;

    #[test]
    fn rewrites_arm_homebrew_cellar_to_symlink() {
        let got = rewrite_homebrew_cellar_to_symlink(Path::new(
            "/opt/homebrew/Cellar/scenery/1.0.45/bin/scenery",
        ));
        assert_eq!(got, Path::new("/opt/homebrew/bin/scenery"));
    }

    #[test]
    fn rewrites_linuxbrew_cellar_to_symlink() {
        let got = rewrite_homebrew_cellar_to_symlink(Path::new(
            "/home/linuxbrew/.linuxbrew/Cellar/scenery/1.0.45/bin/scenery",
        ));
        assert_eq!(got, Path::new("/home/linuxbrew/.linuxbrew/bin/scenery"));
    }

    #[test]
    fn leaves_non_cellar_paths_alone() {
        let got = rewrite_homebrew_cellar_to_symlink(Path::new(
            "/Users/dev/code/scenery/target/release/scenery",
        ));
        assert_eq!(
            got,
            Path::new("/Users/dev/code/scenery/target/release/scenery")
        );
    }
}
