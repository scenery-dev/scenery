//! Scenery runner — core app logic.
//!
//! The app-support directory is `Scenery/` (macOS: `~/Library/Application
//! Support/Scenery/`; Linux: `$XDG_DATA_HOME/scenery/`).
//!
//! # M0 scope
//!
//! - `app` / `state` — bootstrap paths, SQLite schema, Unix socket path helper
//! - `policy` — `PolicyClient` trait + `MockPolicyClient`
//! - `registry` — `RegistryClient` trait + `MockRegistryClient` + `HttpRegistryClient` stub
//! - `audit` — `AuditBuffer` trait + `NoOpAuditBuffer`
//! - `model` — `ModelClient` trait + `MockModelClient`
//!
//! # Deferred to M1+
//!
//! installer, importer, validator, resolver, executor, ollama, auth, updater —
//! none of these are needed by the M0 daemon or shim. They will be ported in M1
//! once the daemon/shim socket protocol is validated end-to-end.

/// Default registry base URL used when no `--registry-url` / `SCENERY_REGISTRY_URL` is set.
pub const DEFAULT_REGISTRY_URL: &str = "https://app.scenery.dev";

pub mod app;
pub mod atomic_file;
pub mod audit;
pub mod binary_path;
pub mod managed;
pub mod model;
pub mod policy;
pub mod registry;
pub mod restore_journal;
pub mod revocation;
pub mod secret_file;
pub mod state;

// M1 modules
pub mod auth;
pub mod executor;
pub mod gateway_model;
pub mod importer;
pub mod installer;
pub mod ollama;
pub mod resolver;
pub mod schema_guard;
pub mod validator;

// `updater` re-enabled in M1.4 — RegistryClient now has the full HTTP impl
// with all methods updater.rs requires (fetch_skill_detail,
// fetch_artifact_metadata, download_artifact, check_skill_status).
pub mod updater;

// M1.2: MCP governance types + HTTP helpers consumed by `scenery-mcp::tools`.
// Some of these helpers will migrate onto `RegistryClient` proper as M1.4's
// trait expansion settles.
pub mod mcp_governance;

// GAP-11: .mcpb Desktop Extension export/import.
pub mod plugin_export;
pub mod plugin_import;

// GAP-05: ratings + execution-count tracking for registry sync.
pub mod ratings;

// SEC3: scan client — `POST /runner/scan` verdict fetch with fail-open semantics.
pub mod scan;

// AUTH2b: heuristic recommendation engine for SKILL.md authoring.
pub mod recommend;
