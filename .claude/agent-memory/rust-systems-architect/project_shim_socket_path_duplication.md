---
name: shim socket path duplication
description: vectorhawkd-shim duplicates daemon_socket_path() locally to avoid pulling in vectorhawkd-core (rusqlite)
type: project
---

`vectorhawkd-shim` deliberately does NOT depend on `vectorhawkd-core`. The `daemon_socket_path()` function (~10 lines) is duplicated in `crates/vectorhawkd-shim/src/lib.rs` rather than imported from `vectorhawkd-core::state`.

**Why:** `vectorhawkd-core` pulls in `rusqlite` with the `bundled` feature (~1.5 MB of linked C code). The shim binary target is <8 MB RSS / ~2-3 MB binary size. Adding core would blow past the binary size budget and violate the "invisible" compute constraint. Without core, the release binary is 1.1 MB.

**How to apply:** If any future change needs to update the socket path convention, it must be updated in BOTH `vectorhawkd-core/src/state.rs` (canonical) AND `vectorhawkd-shim/src/lib.rs` (duplicate). There is a TODO(M1) comment in lib.rs to extract this into a zero-dep `vectorhawkd-paths` crate.
