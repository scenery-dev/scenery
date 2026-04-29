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
