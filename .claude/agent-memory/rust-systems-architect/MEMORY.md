# Memory Index

## Project
- [project_stream2_api_notes.md](./project_stream2_api_notes.md) — Stream 2 API observations: Backend trait import, spawn model, measured ~2.9 MB idle RSS on macOS arm64
- [project_shim_socket_path_duplication.md](./project_shim_socket_path_duplication.md) — shim duplicates daemon_socket_path() to avoid rusqlite dependency; must be kept in sync with vectorhawkd-core
- [project_m1_stream4_registry_audit.md](./project_m1_stream4_registry_audit.md) — M1.4 design: RegistryClient concrete struct, SqliteAuditBuffer, HttpPolicyClient 7-day grace, 300s sync loop
- [project_m1_stream6_validation.md](./project_m1_stream6_validation.md) — M1.6: spawn_blocking audit results, audit-row wiring gap (TODO for M1.7), shim trim 2.96→2.59 MB via `daemon` feature gate
- [project_m2_stream1_install.md](./project_m2_stream1_install.md) — M2.1 install module API, launchctl kickstart quirk on macOS 15+, doctor/mcp-setup integration
- [project_m2_stream2_gate.md](./project_m2_stream2_gate.md) — M2.2: gate 7/8 PASS (AC2 N/A on Darwin), integration test patterns, AC5/AC6 grep anchors, AC3/AC4 derivation from install test
- [project_m3_stream1_listener.md](./project_m3_stream1_listener.md) — M3.1: axum OAuth listener, OAuthState oneshot pub/sub, DaemonContext, reqwest::blocking drop-in-async gotcha in tests
- [project_m3_stream3_cli_flow.md](./project_m3_stream3_cli_flow.md) — M3.3: PKCE primitives, CLI login rewrite (no stdin prompt, exit 2 on daemon missing), daemon refresh_one_tick pub helper
- [project_m3_stream4_gate.md](./project_m3_stream4_gate.md) — M3.4: acceptance gate patterns, macOS timeout workaround, doctor OAuth listener wiring, RSS 8.6 MB, 14 pre-existing clippy errors
