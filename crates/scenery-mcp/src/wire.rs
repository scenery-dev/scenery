//! Daemon-side wiring of the user's AI clients.
//!
//! The daemon calls [`wire_clients_in`] once at start and then on a slow
//! poll. On pkg installs ([`WirePolicy::Enforce`]) the Scenery entry is kept
//! in place. On brew/curl installs ([`WirePolicy::FirstTimeOnly`]) the entry
//! is added once per client and a user's later removal is respected.
//!
//! A pass only stats and reads small config files and writes nothing when
//! every client already has its entry.

use std::path::Path;

use crate::setup::{
    claude_code_supports_http_oauth, detect_ai_clients_in, pick_free_callback_port,
    scenery_entry_present, write_claude_code_entry, write_mcp_entry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WirePolicy {
    /// Always restore a missing entry (pkg installs).
    Enforce,
    /// Add the entry once per client, then leave it alone if removed.
    FirstTimeOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireAction {
    /// Write the entry.
    Write,
    /// Entry already there; just remember the client was wired.
    MarkOnly,
    /// The user removed it and policy says to respect that.
    Skip,
}

/// Remembers which clients have been wired before.
pub trait WireMemory {
    fn was_wired(&self, client: &str) -> bool;
    fn mark_wired(&self, client: &str);
    /// Whether the one-time upgrade baseline has run.
    fn baseline_done(&self) -> bool;
    fn set_baseline_done(&self);
}

/// Meta key recording that the upgrade baseline has run.
const BASELINE_KEY: &str = "mcp_wire_baseline";

/// Whether this pass is the one-time upgrade baseline: `FirstTimeOnly`
/// installs that predate wiring memory must not re-add entries their users
/// removed earlier, so the first pass only records what is there. `Enforce`
/// (pkg) has no such history and ignores it.
pub fn baseline_applies(policy: WirePolicy, baseline_done: bool) -> bool {
    policy == WirePolicy::FirstTimeOnly && !baseline_done
}

/// Pure decision table for one client.
pub fn decide_wire(policy: WirePolicy, entry_present: bool, was_wired: bool) -> WireAction {
    match (entry_present, was_wired, policy) {
        (true, _, _) => WireAction::MarkOnly,
        (false, false, _) => WireAction::Write,
        (false, true, WirePolicy::Enforce) => WireAction::Write,
        (false, true, WirePolicy::FirstTimeOnly) => WireAction::Skip,
    }
}

fn memory_key(client: &str) -> String {
    format!("mcp_wired:{client}")
}

#[cfg(feature = "daemon")]
impl WireMemory for scenery_core::state::AppState {
    fn was_wired(&self, client: &str) -> bool {
        matches!(self.meta_get(&memory_key(client)), Ok(Some(v)) if v == "1")
    }

    fn mark_wired(&self, client: &str) {
        if let Err(e) = self.meta_set(&memory_key(client), "1") {
            tracing::warn!(client, error = %e, "wire: could not record wired client");
        }
    }

    fn baseline_done(&self) -> bool {
        matches!(self.meta_get(BASELINE_KEY), Ok(Some(v)) if v == "1")
    }

    fn set_baseline_done(&self) {
        if let Err(e) = self.meta_set(BASELINE_KEY, "1") {
            tracing::warn!(error = %e, "wire: could not record upgrade baseline");
        }
    }
}

/// One wiring pass over every detected AI client. Returns the names of the
/// clients whose config was written.
///
/// `claude_http` is the daemon's local MCP HTTP `(port, local key)`; Claude
/// Code gets the HTTP+OAuth entry when it is `Some` and the installed Claude
/// Code supports it, otherwise the stdio entry. Per-client failures are
/// logged and skipped.
pub fn wire_clients_in(
    home: &Path,
    system_root: &Path,
    policy: WirePolicy,
    mem: &dyn WireMemory,
    claude_http: Option<(u16, String)>,
) -> Vec<String> {
    let mut written = Vec::new();
    let baseline_pass = baseline_applies(policy, mem.baseline_done());
    for client in detect_ai_clients_in(home, system_root) {
        if baseline_pass {
            if !mem.was_wired(&client.name) {
                mem.mark_wired(&client.name);
            }
            continue;
        }
        let present = entry_present_memo(&client.config_path, &client.mcp_key, client.format);
        let was_wired = mem.was_wired(&client.name);
        match decide_wire(policy, present, was_wired) {
            WireAction::Skip => {}
            WireAction::MarkOnly => {
                if !was_wired {
                    mem.mark_wired(&client.name);
                }
            }
            WireAction::Write => {
                let result = if client.name == "Claude Code" {
                    write_claude_code_entry(
                        &client,
                        claude_http.clone(),
                        claude_code_supports_http_oauth,
                        pick_free_callback_port,
                    )
                    .map(|_| ())
                } else {
                    write_mcp_entry(&client)
                };
                match result {
                    Ok(()) => {
                        mem.mark_wired(&client.name);
                        written.push(client.name.clone());
                    }
                    Err(e) => tracing::warn!(
                        client = %client.name,
                        path = %client.config_path.display(),
                        error = %e,
                        "wire: failed to write scenery MCP entry"
                    ),
                }
            }
        }
    }
    if baseline_pass {
        mem.set_baseline_done();
    }
    written
}

// ── Entry check memo ─────────────────────────────────────────────────────────
//
// The poll asks every minute whether each client's config still holds the
// Scenery entry, which parsed the whole file each time. `~/.claude.json`
// carries Claude Code's project history and can grow to many megabytes. The
// answer is now remembered per file against a metadata stamp (size, mtime,
// and on Unix ctime and inode: any write or replacement changes one), and
// only for a file untouched for RACY_WINDOW, so a write landing within the
// filesystem's timestamp granularity is never missed.

const RACY_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    mtime: Option<std::time::SystemTime>,
    ctime_ns: i128,
    ino: u64,
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let m = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let (ctime_ns, ino) = {
        use std::os::unix::fs::MetadataExt;
        (
            i128::from(m.ctime()) * 1_000_000_000 + i128::from(m.ctime_nsec()),
            m.ino(),
        )
    };
    #[cfg(not(unix))]
    let (ctime_ns, ino) = (0i128, 0u64);
    Some(FileStamp {
        len: m.len(),
        mtime: m.modified().ok(),
        ctime_ns,
        ino,
    })
}

fn settled(stamp: &FileStamp) -> bool {
    let now = std::time::SystemTime::now();
    let old_enough = |t: std::time::SystemTime| {
        now.duration_since(t)
            .map(|age| age >= RACY_WINDOW)
            .unwrap_or(false)
    };
    let ctime_ok = {
        #[cfg(unix)]
        {
            let secs = stamp.ctime_ns.div_euclid(1_000_000_000);
            let nanos = stamp.ctime_ns.rem_euclid(1_000_000_000);
            match (u64::try_from(secs), u32::try_from(nanos)) {
                (Ok(s), Ok(n)) => {
                    old_enough(std::time::UNIX_EPOCH + std::time::Duration::new(s, n))
                }
                _ => false,
            }
        }
        #[cfg(not(unix))]
        {
            true
        }
    };
    stamp.mtime.is_some_and(old_enough) && ctime_ok
}

type PresentMemo =
    std::sync::Mutex<std::collections::HashMap<(std::path::PathBuf, String), (FileStamp, bool)>>;

fn present_memo() -> &'static PresentMemo {
    static MEMO: std::sync::OnceLock<PresentMemo> = std::sync::OnceLock::new();
    MEMO.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// [`scenery_entry_present`], re-read only when the file changed.
pub(crate) fn entry_present_memo(
    path: &Path,
    mcp_key: &str,
    format: crate::setup::ConfigFormat,
) -> bool {
    let Some(stamp) = file_stamp(path) else {
        return scenery_entry_present(path, mcp_key, format);
    };
    let key = (path.to_path_buf(), mcp_key.to_string());
    if let Ok(m) = present_memo().lock() {
        if let Some((s, v)) = m.get(&key) {
            if *s == stamp {
                return *v;
            }
        }
    }
    let v = scenery_entry_present(path, mcp_key, format);
    if settled(&stamp) {
        if let Ok(mut m) = present_memo().lock() {
            if m.len() >= 256 {
                m.clear();
            }
            m.insert(key, (stamp, v));
        }
    }
    v
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
