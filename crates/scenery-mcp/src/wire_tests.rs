use super::*;
use crate::setup::HOME_MUTEX;
use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Default)]
struct MemMemory {
    wired: RefCell<HashSet<String>>,
    baseline: std::cell::Cell<bool>,
}

impl MemMemory {
    /// Memory as it is after the upgrade baseline already ran.
    fn baselined() -> Self {
        let m = Self::default();
        m.baseline.set(true);
        m
    }
}

impl WireMemory for MemMemory {
    fn was_wired(&self, client: &str) -> bool {
        self.wired.borrow().contains(client)
    }
    fn baseline_done(&self) -> bool {
        self.baseline.get()
    }
    fn set_baseline_done(&self) {
        self.baseline.set(true);
    }
    fn mark_wired(&self, client: &str) {
        self.wired.borrow_mut().insert(client.to_string());
    }
}

fn temp_home(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("scn-wire-{label}-{nanos}"));
    fs::create_dir_all(&p).unwrap();
    p
}

/// Run with HOME pointed at `home` so the restore journal that
/// `write_mcp_entry` appends to never touches the real data dir.
fn with_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
    let _g = HOME_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let orig = std::env::var_os("HOME");
    std::env::set_var("HOME", home);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    match orig {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    match r {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(e),
    }
}

fn cursor_config(home: &Path) -> PathBuf {
    home.join(".cursor").join("mcp.json")
}

fn has_scenery(path: &Path) -> bool {
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    v.get("mcpServers").and_then(|m| m.get("scenery")).is_some()
}

fn make_cursor(home: &Path) {
    fs::create_dir_all(home.join(".cursor")).unwrap();
    fs::write(cursor_config(home), "{}").unwrap();
}

fn wire(home: &Path, policy: WirePolicy, mem: &MemMemory) -> Vec<String> {
    with_home(home, || wire_clients_in(home, home, policy, mem, None))
}

#[test]
fn decide_wire_table() {
    use WireAction::*;
    use WirePolicy::*;
    for p in [Enforce, FirstTimeOnly] {
        assert_eq!(decide_wire(p, true, false), MarkOnly);
        assert_eq!(decide_wire(p, true, true), MarkOnly);
        assert_eq!(decide_wire(p, false, false), Write);
    }
    assert_eq!(decide_wire(Enforce, false, true), Write);
    assert_eq!(decide_wire(FirstTimeOnly, false, true), Skip);
}

#[test]
fn brew_mode_respects_user_removal() {
    let home = temp_home("brew-removal");
    make_cursor(&home);
    let mem = MemMemory::baselined();
    assert_eq!(wire(&home, WirePolicy::FirstTimeOnly, &mem), ["Cursor"]);
    assert!(has_scenery(&cursor_config(&home)));

    fs::write(cursor_config(&home), r#"{"mcpServers":{}}"#).unwrap();
    assert!(wire(&home, WirePolicy::FirstTimeOnly, &mem).is_empty());
    assert!(!has_scenery(&cursor_config(&home)));
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn pkg_mode_restores_removed_entry() {
    let home = temp_home("pkg-restore");
    make_cursor(&home);
    let mem = MemMemory::default();
    wire(&home, WirePolicy::Enforce, &mem);
    fs::write(cursor_config(&home), r#"{"mcpServers":{}}"#).unwrap();
    assert_eq!(wire(&home, WirePolicy::Enforce, &mem), ["Cursor"]);
    assert!(has_scenery(&cursor_config(&home)));
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn existing_entry_is_marked_not_rewritten() {
    let home = temp_home("existing");
    make_cursor(&home);
    let existing =
        r#"{"mcpServers":{"scenery":{"command":"/somewhere/scenery","args":["mcp","serve"]}}}"#;
    fs::write(cursor_config(&home), existing).unwrap();
    let mem = MemMemory::default();
    assert!(wire(&home, WirePolicy::FirstTimeOnly, &mem).is_empty());
    assert!(mem.was_wired("Cursor"));
    assert_eq!(fs::read_to_string(cursor_config(&home)).unwrap(), existing);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn wire_pass_picks_up_newly_installed_client() {
    let home = temp_home("new-client");
    let mem = MemMemory::baselined();
    assert!(!wire(&home, WirePolicy::FirstTimeOnly, &mem).contains(&"Cursor".to_string()));
    make_cursor(&home);
    assert!(wire(&home, WirePolicy::FirstTimeOnly, &mem).contains(&"Cursor".to_string()));
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn baseline_applies_only_to_first_time_policy_without_baseline() {
    use WirePolicy::*;
    assert!(baseline_applies(FirstTimeOnly, false));
    assert!(!baseline_applies(FirstTimeOnly, true));
    assert!(!baseline_applies(Enforce, false));
}

#[test]
fn first_brew_pass_after_upgrade_does_not_readd_removed_entry() {
    let home = temp_home("baseline-removed");
    make_cursor(&home); // config exists, no scenery entry (user removed it)
    let mem = MemMemory::default();
    assert!(wire(&home, WirePolicy::FirstTimeOnly, &mem).is_empty());
    assert!(!has_scenery(&cursor_config(&home)));
    assert!(mem.was_wired("Cursor"));
    assert!(mem.baseline_done());
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn client_installed_after_baseline_is_wired() {
    let home = temp_home("baseline-new-client");
    let mem = MemMemory::default();
    assert!(wire(&home, WirePolicy::FirstTimeOnly, &mem).is_empty());
    assert!(mem.baseline_done());
    make_cursor(&home);
    assert_eq!(wire(&home, WirePolicy::FirstTimeOnly, &mem), ["Cursor"]);
    assert!(has_scenery(&cursor_config(&home)));
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn enforce_ignores_baseline() {
    let home = temp_home("baseline-enforce");
    make_cursor(&home);
    let mem = MemMemory::default();
    assert_eq!(wire(&home, WirePolicy::Enforce, &mem), ["Cursor"]);
    assert!(has_scenery(&cursor_config(&home)));
    assert!(!mem.baseline_done());
    let _ = fs::remove_dir_all(&home);
}
