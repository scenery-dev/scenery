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
pub mod model;
pub mod policy;
pub mod registry;
pub mod state;

// M1 modules — ported from skillrunner-core
pub mod auth;
pub mod executor;
pub mod importer;
pub mod installer;
pub mod ollama;
pub mod resolver;
pub mod validator;

// `updater` deferred to M1.4 — depends on RegistryClient trait methods
// (fetch_skill_detail, fetch_artifact_metadata, download_artifact,
// check_skill_status) that aren't in the M0 trait surface. The full HTTP
// registry port happens in M1.4 (audit + policy + sync), which owns
// `registry.rs` end-to-end. Leaving the ported file in the tree but
// excluded from compilation so M1.4 can pick it up.
// pub mod updater;
