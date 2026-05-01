---
name: Stream 2 API observations from Stream 4 daemon integration
description: What Stream 4 found when consuming Stream 2 (vectorhawkd-mcp) — trait import requirements, spawn model, RSS result
type: project
---

`read_framed` / `write_framed` are exported from `vectorhawkd_mcp::backend` and work correctly. No missing helpers.

`Backend` trait methods (`initialize`, `list_tools`, `call_tool`, `on_shutdown`) are NOT auto-imported when holding a concrete type (`Arc<RealBackend>`). Callers must explicitly `use vectorhawkd_mcp::backend::Backend;` in scope.

`tokio::spawn` (not `spawn_local`) is correct for per-connection tasks because `RealBackend: Send + Sync`. `spawn_local` would require a `LocalSet` wrapper and is unnecessary overhead.

Daemon idle RSS at release (stripped, current-thread, macOS arm64): **~2.9 MB**. Well inside the 50 MB budget. The budget spec (35–45 MB) has enormous headroom for M1 features.

**Why:** Load-bearing for M1 planning — don't add heavy init paths (full reqwest client, CA cert loading) at startup. Keep lazy-dial pattern.

**How to apply:** When adding M1 registry sync or policy cache client, initialize them lazily on first use rather than at daemon boot. The startup path is currently O(100 µs) and should stay that way.
