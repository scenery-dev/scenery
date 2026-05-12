//! VectorHawk runner — core app logic.
//!
//! Ported from `skillrunner-core`. The app-support directory is renamed from
//! `SkillClub/SkillRunner/` to `VectorHawk/` (macOS: `~/Library/Application
//! Support/VectorHawk/`; Linux: `$XDG_DATA_HOME/vectorhawk/`).
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

pub mod app;
pub mod audit;
pub mod managed;
pub mod model;
pub mod policy;
pub mod registry;
pub mod state;

// M1 modules — ported from skillrunner-core
pub mod auth;
pub mod executor;
pub mod gateway_model;
pub mod importer;
pub mod installer;
pub mod ollama;
pub mod resolver;
pub mod validator;

// `updater` re-enabled in M1.4 — RegistryClient now has the full HTTP impl
// with all methods updater.rs requires (fetch_skill_detail,
// fetch_artifact_metadata, download_artifact, check_skill_status).
pub mod updater;

// M1.2: MCP governance types + HTTP helpers consumed by `vectorhawkd-mcp::tools`.
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
