---
name: M1.4 registry/audit architectural decisions
description: Key design choices from M1.4 — RegistryClient concrete struct, SqliteAuditBuffer, HttpPolicyClient, sync loop
type: project
---

RegistryClient changed from a 4-method trait to a concrete struct (matching skillrunner-core pattern). MockRegistryClient kept as a plain struct (not a trait impl) for test use.

**Why:** updater.rs used &dyn RegistryClient with 8+ methods that weren't on the M0 trait. Turning it into a concrete struct is simpler, avoids trait object overhead, and matches the skillrunner precedent. Tests using MockRegistryClient call it directly rather than through a trait.

**How to apply:** When new callers need a swappable registry (e.g. future test isolation), pass `&RegistryClient` directly. If true trait polymorphism is needed later, add a trait then.

SqliteAuditBuffer owns a `Mutex<usize>` pending_count for the 100-event threshold. The Mutex is locked briefly in record() — not across any I/O. The flush() is intentionally synchronous; callers must use spawn_blocking.

HttpPolicyClient lives in registry.rs (not policy.rs). It holds a RegistryClient and a db_path, implements PolicyClient trait. 7-day grace: compares fetched_at timestamp in policy_cache against unix_now().

AppState::list_installed_skill_ids() added (synchronous SQLite read) so the daemon sync loop can avoid a direct rusqlite dependency in the daemon crate.

Registry sync loop in daemon: 300s tokio::time::interval, first tick skipped to let accept loop start, all I/O in spawn_blocking. Steps: audit flush → fetch_approved_servers → check_skill_status. Each step failure logs WARN and continues.

Final audit flush on SIGTERM: spawn_blocking call at end of run_daemon before socket cleanup.
