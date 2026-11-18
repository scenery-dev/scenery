//! Authoring with Scenery (plan 4.7): `scenery_create_skill`,
//! `scenery_check_skill` and `scenery_share_skill`.
//!
//! * **create**: scaffold `~/.agents/skills/<slug>/`, then register it as the
//!   user's own private skill and convert it in place at once (the daemon's
//!   [`scenery_core::authoring::CreateHook`]), so it is managed from its first
//!   save. Offline: the folder stays and discovery assigns its ID later.
//! * **check**: offline layout + secret check, then a scan preview from the
//!   registry (nothing stored as a skill).
//! * **share**: two steps. Step one checks, previews, uploads the current tree
//!   as a new private version and returns a preview plus a single-use
//!   confirmation token bound to the tree hash. Step two, with the token,
//!   publishes through the existing share path. The token is handed to the AI
//!   client, so it cannot prove a human agreed: an injected agent could call step
//!   two itself. The server has no way to ask the human directly (it is a
//!   hand-rolled JSON-RPC server; the daemon relay and the HTTP transport carry
//!   no server-to-client requests, so MCP elicitation is not available), so
//!   step two ALWAYS submits with `force_review`: the share goes to IT's review
//!   queue whatever the org's approval mode, never straight to the catalog.
//!
//! Guidance, not enforcement: nothing here blocks a manual path.

use crate::protocol::{ToolCallResult, ToolDefinition};
use rusqlite::Connection;
use scenery_core::{
    authoring::{self, CreateError},
    registry::{AuthoringReply, RegisterSkillRequest, RegistryClient},
    state::AppState,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

/// How long a share confirmation token stays valid.
const TOKEN_TTL: Duration = Duration::from_secs(15 * 60);

pub const TOOL_NAMES: [&str; 3] = [
    "scenery_create_skill",
    "scenery_check_skill",
    "scenery_share_skill",
];

#[cfg(test)]
thread_local! {
    /// Test-only home override (a per-thread stand-in for `$HOME`, which other
    /// tests in this crate also rewrite under their own lock).
    pub(crate) static TEST_HOME: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

fn agents_skills_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(h) = TEST_HOME.with(|t| t.borrow().clone()) {
        return Some(h.join(".agents").join("skills"));
    }
    crate::ownership::agents_skills_dir()
}

fn claude_skills_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(h) = TEST_HOME.with(|t| t.borrow().clone()) {
        return Some(h.join(".claude").join("skills"));
    }
    crate::ownership::claude_skills_dir()
}

pub fn is_authoring_tool(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}

pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "scenery_create_skill".to_string(),
            description: "Create a new skill the user asked for. Writes it to \
                ~/.agents/skills/<name>/ and registers it as the user's own PRIVATE skill so it \
                is governed from its first save; it is linked for Claude Code at once. Use this \
                instead of writing skill files by hand."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Short folder name, e.g. 'meeting-notes' (letters, digits, '-')"
                    },
                    "description": {
                        "type": "string",
                        "description": "One or two sentences on what the skill does and WHEN to use it"
                    },
                    "instructions": {
                        "type": "string",
                        "description": "The skill body in Markdown: what the AI should do when the skill applies"
                    }
                },
                "required": ["name", "description", "instructions"]
            }),
        },
        ToolDefinition {
            name: "scenery_check_skill".to_string(),
            description: "Check a skill before it is called done or shared: layout, secrets in \
                the folder, and a preview of the security scan. Nothing is stored or shared."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The skill's folder name under ~/.agents/skills"},
                    "path": {"type": "string", "description": "Alternatively, the path of a skill folder"}
                },
                "required": []
            }),
        },
        ToolDefinition {
            name: "scenery_share_skill".to_string(),
            description: "Share the user's own skill with their organization. ONLY when the user \
                asks to share. Two steps: the first call checks the skill, uploads it privately \
                and returns a PREVIEW and a confirm_token. Show the preview to the user in plain \
                words and ask. Only after they agree, call again with the confirm_token. Never \
                confirm on your own."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The skill's folder name"},
                    "catalog_name": {
                        "type": "string",
                        "description": "Name to publish under in the company catalog (default: the skill name). Use it when a preview says name_clash."
                    },
                    "confirm_token": {
                        "type": "string",
                        "description": "From the preview, after the user agreed. Single use."
                    },
                    "group_ids": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Optional: share with these groups only instead of the whole organization"
                    }
                },
                "required": ["name"]
            }),
        },
    ]
}

pub fn handle(
    name: &str,
    args: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    match name {
        "scenery_create_skill" => handle_create(args, state, registry_url),
        "scenery_check_skill" => handle_check(args, state, registry_url),
        "scenery_share_skill" => handle_share(args, state, registry_url),
        other => ToolCallResult::error_result(format!("unknown authoring tool {other}")),
    }
}

fn registry(registry_url: &Option<String>) -> String {
    registry_url
        .clone()
        .unwrap_or_else(|| scenery_core::DEFAULT_REGISTRY_URL.to_string())
}

fn str_arg<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn json_result(v: serde_json::Value) -> ToolCallResult {
    ToolCallResult::success(serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()))
}

fn json_error(v: serde_json::Value) -> ToolCallResult {
    ToolCallResult::error_result(serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()))
}

// ── create ──────────────────────────────────────────────────────────────────

fn handle_create(
    args: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let (Some(raw_name), Some(description), Some(instructions)) = (
        str_arg(args, "name"),
        str_arg(args, "description"),
        str_arg(args, "instructions"),
    ) else {
        return ToolCallResult::error_result(
            "Missing required parameters: name, description and instructions are all required.",
        );
    };
    if let Some(why) = authoring::slug_error(raw_name) {
        return ToolCallResult::error_result(format!(
            "Cannot use '{raw_name}' as a skill name: {why}."
        ));
    }
    if description.to_ascii_uppercase().starts_with("TODO") {
        return ToolCallResult::error_result(
            "The description must say what the skill does and when to use it, not a TODO.",
        );
    }
    let slug = authoring::normalize_slug(raw_name);
    let Some(root) = agents_skills_dir() else {
        return ToolCallResult::error_result("Could not resolve the home directory.");
    };
    let dir = root.join(&slug);
    let claude_link = claude_skills_dir().map(|d| d.join(&slug));
    // symlink_metadata: a dangling link counts as taken. Never overwrite anything.
    if dir.symlink_metadata().is_ok()
        || claude_link
            .as_ref()
            .is_some_and(|p| p.symlink_metadata().is_ok())
    {
        return ToolCallResult::error_result(format!(
            "A skill named '{slug}' already exists on this machine. Pick another name, or edit \
             the existing skill in {}.",
            dir.display()
        ));
    }
    let md = authoring::skill_md(&slug, description, instructions);
    if let Err(e) = write_new_skill(&root, &dir, &md) {
        return ToolCallResult::error_result(format!("Could not create the skill folder: {e:#}"));
    }
    // A pasted key in the instructions must not be registered or uploaded.
    let check = authoring::check_tree(&dir);
    if !check.secrets.is_empty() {
        let _ = std::fs::remove_dir_all(&dir);
        return ToolCallResult::error_result(
            "The instructions contain what looks like a secret (an API key, token or private \
             key). Nothing was created. Remove it and keep secrets out of skills.",
        );
    }

    let url = registry(registry_url);
    let base = serde_json::json!({
        "name": slug,
        "path": dir.display().to_string(),
        "private": true,
    });
    let merge = |extra: serde_json::Value| {
        let mut v = base.clone();
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        v
    };
    match authoring::run_create_hook(state, &url, &slug) {
        None => json_result(merge(serde_json::json!({
            "status": "created_locally",
            "message": "Created. Scenery will register and manage it at its next scan.",
        }))),
        Some(Ok(c)) => json_result(merge(serde_json::json!({
            "status": "managed",
            "skill_id": c.skill_id,
            "version": c.version,
            "scan": c.scan_verdict,
            "message": "Created, registered as your own private skill and linked for your AI \
                        clients. Only you can see it. Edit the files in the folder to refine it, \
                        then call scenery_check_skill.",
        }))),
        Some(Err(CreateError::Offline(why))) => json_result(merge(serde_json::json!({
            "status": "created_locally",
            "message": format!(
                "Created locally ({why}). Scenery registers and manages it at its next scan \
                 once you are signed in."
            ),
        }))),
        Some(Err(CreateError::Refused { code, message })) => {
            json_result(merge(serde_json::json!({
                "status": "created_not_registered",
                "reason": code,
                "message": format!(
                    "The folder was created but the registry did not accept it: {message}. Fix the \
                     problem and call scenery_check_skill."
                ),
            })))
        }
        Some(Err(CreateError::Failed(why))) => json_result(merge(serde_json::json!({
            "status": "created_not_registered",
            "message": format!(
                "The folder was created, but registering or converting it failed: {why}. \
                 Scenery will retry at its next scan."
            ),
        }))),
    }
}

/// Create `dir` (and the shared root) and write SKILL.md. `create_dir` on the
/// leaf fails if anything appeared there since the check (a planted symlink
/// included), so the file lands in a fresh directory.
fn write_new_skill(root: &Path, dir: &Path, skill_md: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(root)?;
    std::fs::create_dir(dir)?;
    if let Err(e) = std::fs::write(dir.join("SKILL.md"), skill_md) {
        let _ = std::fs::remove_dir_all(dir);
        return Err(e.into());
    }
    Ok(())
}

// ── resolving a skill folder ────────────────────────────────────────────────

/// Canonicalize `p` and accept it only when it is a skill folder directly
/// under one of the skill roots (`~/.agents/skills`, `~/.claude/skills`).
/// `canonicalize` resolves `..` and symlinks first, so a link inside a root that
/// points elsewhere (or a path outside the roots) is refused: an injected agent
/// cannot make check/share pack and upload an arbitrary directory.
fn confine_to_skill_roots(p: &Path) -> Result<PathBuf, String> {
    let not_allowed = || {
        format!(
            "'{}' is not a skill folder under ~/.agents/skills. Give the skill's `name` instead.",
            p.display()
        )
    };
    let canon =
        std::fs::canonicalize(p).map_err(|_| format!("'{}' is not a folder.", p.display()))?;
    if !canon.is_dir() {
        return Err(format!("'{}' is not a folder.", p.display()));
    }
    let roots = [agents_skills_dir(), claude_skills_dir()];
    for root in roots.into_iter().flatten() {
        let Ok(root_canon) = std::fs::canonicalize(&root) else {
            continue;
        };
        if canon.parent() == Some(root_canon.as_path()) {
            // Hand back the logical `<root>/<slug>`: the marker rows are keyed
            // by that spelling, not by the canonical one.
            if let Some(name) = canon.file_name() {
                return Ok(root.join(name));
            }
        }
    }
    Err(not_allowed())
}

/// `(slug, dir)` from `name` (under `~/.agents/skills`) or `path`.
fn resolve_skill_dir(args: &serde_json::Value) -> Result<(String, PathBuf), String> {
    if let Some(p) = str_arg(args, "path") {
        let dir = confine_to_skill_roots(Path::new(p))?;
        let slug = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        return Ok((slug, dir));
    }
    let Some(name) = str_arg(args, "name") else {
        return Err("Give the skill's `name` (or a `path`).".to_string());
    };
    if let Some(why) = authoring::slug_error(name) {
        // The built-in skill is Scenery's own: never checked or shared.
        return Err(format!("'{name}' is not a usable skill name: {why}."));
    }
    let slug = authoring::normalize_slug(name);
    let root = agents_skills_dir().ok_or("Could not resolve the home directory.")?;
    let dir = root.join(&slug);
    if !dir.is_dir() {
        return Err(format!("No skill folder at {}.", dir.display()));
    }
    Ok((slug, dir))
}

/// The registry identity the runner recorded for this folder: an auto-managed
/// own skill (marker row) or a tracked dev skill. `(skill_id, skill_uuid)`.
fn registry_identity(state: &AppState, dir: &Path) -> Option<(String, String)> {
    let conn = Connection::open(&state.db_path).ok()?;
    let key = dir.to_string_lossy().to_string();
    let canon = std::fs::canonicalize(dir)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| key.clone());
    for k in [&key, &canon] {
        let row: Option<(Option<String>, Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT origin, skill_id, skill_uuid FROM managed_path_markers \
                 WHERE path = ?1 AND kind = 'skill'",
                [k],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok();
        if let Some((Some(origin), Some(id), Some(uuid))) = row {
            if origin == "auto_managed" && !uuid.is_empty() {
                return Some((id, uuid));
            }
        }
        let tracked: Option<(Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT skill_id, skill_uuid FROM tracked_skills WHERE path = ?1",
                [k],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        if let Some((Some(id), Some(uuid))) = tracked {
            if !uuid.is_empty() {
                return Some((id, uuid));
            }
        }
    }
    None
}

fn is_builtin_dir(state: &AppState, dir: &Path) -> bool {
    let Ok(conn) = Connection::open(&state.db_path) else {
        return false;
    };
    let origin: Option<Option<String>> = conn
        .query_row(
            "SELECT origin FROM managed_path_markers WHERE path = ?1 AND kind = 'skill'",
            [dir.to_string_lossy().to_string()],
            |r| r.get(0),
        )
        .ok();
    matches!(origin, Some(Some(o)) if o == "builtin")
}

fn pack(dir: &Path) -> Result<Vec<u8>, String> {
    scenery_core::pack::pack_dir_to_tar_gz_no_links(dir).map_err(|e| format!("{e:#}"))
}

fn check_json(c: &authoring::TreeCheck) -> serde_json::Value {
    serde_json::json!({
        "ok": c.ok(),
        "files": c.files,
        "problems": c.problems,
        "warnings": c.warnings,
        "secret_files": c.secrets,
    })
}

// ── check ───────────────────────────────────────────────────────────────────

fn handle_check(
    args: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let (slug, dir) = match resolve_skill_dir(args) {
        Ok(v) => v,
        Err(e) => return ToolCallResult::error_result(e),
    };
    if is_builtin_dir(state, &dir) || slug == authoring::BUILTIN_SLUG {
        return ToolCallResult::error_result(
            "authoring-with-scenery is built into Scenery; there is nothing to check or share.",
        );
    }
    let check = authoring::check_tree(&dir);
    let mut out = serde_json::json!({
        "name": slug,
        "path": dir.display().to_string(),
        "layout": check_json(&check),
    });
    if !check.ok() {
        out["status"] = "needs_fixes".into();
        out["message"] =
            "Fix the problems above (secrets are listed by file name only), then check again."
                .into();
        return json_result(out);
    }
    let url = registry(registry_url);
    let token = match scenery_core::auth::load_tokens(state, &url) {
        Ok(Some(t)) => t.access_token,
        _ => {
            out["status"] = "layout_ok".into();
            out["scan"] = serde_json::json!({"available": false, "reason": "not signed in"});
            out["message"] =
                "The layout is fine. Sign in (scenery_login) to preview the security scan.".into();
            return json_result(out);
        }
    };
    let tar = match pack(&dir) {
        Ok(t) => t,
        Err(e) => {
            out["status"] = "needs_fixes".into();
            out["message"] = format!("Could not pack the skill: {e}").into();
            return json_result(out);
        }
    };
    let client = RegistryClient::new(&url).with_auth(token);
    match client.preview_scan(tar, &slug, None) {
        Ok(AuthoringReply::Done(p)) => {
            out["status"] = if p.secrets.is_empty() {
                "ok"
            } else {
                "needs_fixes"
            }
            .into();
            out["scan"] = serde_json::json!({
                "verdict": p.verdict,
                "decision": p.decision,
                "findings": p.findings,
                "it_review_would_apply": p.requires_review,
            });
            if !p.secrets.is_empty() {
                out["layout"]["secret_files"] = serde_json::json!(p.secrets);
            }
            json_result(out)
        }
        Ok(AuthoringReply::Refused(r)) => {
            out["status"] = "layout_ok".into();
            out["scan"] = serde_json::json!({"available": false, "reason": format!("{} {}", r.code, r.message).trim()});
            json_result(out)
        }
        Err(e) => {
            out["status"] = "layout_ok".into();
            out["scan"] = serde_json::json!({"available": false, "reason": format!("registry not reachable: {e:#}")});
            json_result(out)
        }
    }
}

// ── share ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct PendingShare {
    slug: String,
    skill_uuid: String,
    tree_hash: String,
    catalog_name: String,
    version: String,
    groups: Vec<String>,
    expires: Instant,
}

fn tokens() -> &'static Mutex<HashMap<String, PendingShare>> {
    static T: OnceLock<Mutex<HashMap<String, PendingShare>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn mint_token(p: PendingShare) -> String {
    let token = format!("shr_{}", uuid::Uuid::new_v4().simple());
    if let Ok(mut g) = tokens().lock() {
        let now = Instant::now();
        g.retain(|_, v| v.expires > now);
        g.insert(token.clone(), p);
    }
    token
}

/// Single use: the token is gone after this call, whatever it answers.
fn take_token(token: &str) -> Result<PendingShare, &'static str> {
    let mut g = tokens().lock().map_err(|_| "token store unavailable")?;
    let p = g.remove(token).ok_or("unknown or already used")?;
    if p.expires <= Instant::now() {
        return Err("expired");
    }
    Ok(p)
}

fn handle_share(
    args: &serde_json::Value,
    state: &AppState,
    registry_url: &Option<String>,
) -> ToolCallResult {
    let (slug, dir) = match resolve_skill_dir(args) {
        Ok(v) => v,
        Err(e) => return ToolCallResult::error_result(e),
    };
    if is_builtin_dir(state, &dir) || slug == authoring::BUILTIN_SLUG {
        return ToolCallResult::error_result(
            "authoring-with-scenery is built into Scenery and cannot be shared.",
        );
    }
    let url = registry(registry_url);
    let token = match scenery_core::auth::load_tokens(state, &url) {
        Ok(Some(t)) => t.access_token,
        _ => {
            return ToolCallResult::error_result(
                "Sharing needs you to be signed in. Use the `scenery_login` tool first.",
            )
        }
    };
    let Some((_skill_id, skill_uuid)) = registry_identity(state, &dir) else {
        return ToolCallResult::error_result(format!(
            "'{slug}' is not registered with Scenery yet (it has no registry ID). Create it with \
             scenery_create_skill, or wait for Scenery's next scan of this machine, then try again."
        ));
    };
    let catalog_name = authoring::normalize_slug(str_arg(args, "catalog_name").unwrap_or(&slug));
    if let Some(why) = authoring::slug_error(&catalog_name) {
        return ToolCallResult::error_result(format!(
            "'{catalog_name}' cannot be a catalog name: {why}. Pick another with catalog_name."
        ));
    }
    let groups: Vec<String> = args
        .get("group_ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|g| g.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let check = authoring::check_tree(&dir);
    let client = RegistryClient::new(&url).with_auth(token);

    // ── step two: confirm ──
    if let Some(t) = str_arg(args, "confirm_token") {
        let pending = match take_token(t) {
            Ok(p) => p,
            Err(why) => {
                return json_error(serde_json::json!({
                    "status": "confirmation_refused",
                    "reason": why,
                    "message": "The confirmation token is not valid. Run the first step again and \
                                show the new preview to the user.",
                }))
            }
        };
        if pending.slug != slug
            || pending.skill_uuid != skill_uuid
            || pending.catalog_name != catalog_name
            || pending.groups != groups
        {
            return json_error(serde_json::json!({
                "status": "confirmation_refused",
                "reason": "the token was issued for a different skill, name or audience",
                "message": "Run the first step again.",
            }));
        }
        if !check.ok() || check.tree_hash != pending.tree_hash {
            return json_error(serde_json::json!({
                "status": "confirmation_refused",
                "reason": "the skill changed after the preview",
                "message": "The skill changed since the user saw the preview. Run the first step \
                            again so they can review what will actually be shared.",
            }));
        }
        return finish_share(&client, &pending);
    }

    // ── step one: check, preview, upload, mint the token ──
    if !check.ok() {
        return json_error(serde_json::json!({
            "status": "needs_fixes",
            "layout": check_json(&check),
            "message": "Fix these before sharing (secrets are listed by file name only).",
        }));
    }
    let tar = match pack(&dir) {
        Ok(t) => t,
        Err(e) => return ToolCallResult::error_result(format!("Could not pack the skill: {e}")),
    };
    let preview = match client.preview_scan(tar.clone(), &slug, Some(&catalog_name)) {
        Ok(AuthoringReply::Done(p)) => p,
        Ok(AuthoringReply::Refused(r)) => return refusal("preview", &r),
        Err(e) => {
            return ToolCallResult::error_result(format!("Could not reach the registry: {e:#}"))
        }
    };
    if !preview.secrets.is_empty() {
        return json_error(serde_json::json!({
            "status": "needs_fixes",
            "secret_files": preview.secrets,
            "message": "The registry's check found files that look like secrets. Remove them first.",
        }));
    }
    if preview.name_clash {
        return json_result(serde_json::json!({
            "status": "name_clash",
            "catalog_name": catalog_name,
            "suggestions": preview.suggestions,
            "message": format!(
                "A catalog skill named '{catalog_name}' already exists. Ask the user for a \
                 different catalog name and call this tool again with catalog_name."
            ),
        }));
    }
    if preview.decision.as_deref() == Some("reject") {
        return json_error(serde_json::json!({
            "status": "blocked",
            "reason": "scan_rejected",
            "scan": {"verdict": preview.verdict, "findings": preview.findings},
            "message": "The security scan rejected this skill; it cannot be shared.",
        }));
    }
    if preview.verdict == "unknown" {
        return json_error(serde_json::json!({
            "status": "scan_unavailable",
            "message": "The security scan is unavailable right now; try again shortly.",
        }));
    }
    let Some(device_id) = state.get_sync_state("device_id").ok().flatten() else {
        return ToolCallResult::error_result("This device is not paired yet; it cannot upload.");
    };
    // Upload the current tree as a new private version (no install row, so a
    // registry copy is never pushed over the user's files).
    let uploaded = match client.register_skill(RegisterSkillRequest {
        tar_gz: tar,
        slug: &slug,
        tree_hash: &check.tree_hash,
        device_id: &device_id,
        mode: "track",
    }) {
        Ok(AuthoringReply::Done(r)) => r,
        Ok(AuthoringReply::Refused(r)) => return refusal("upload", &r),
        Err(e) => {
            return ToolCallResult::error_result(format!("Could not reach the registry: {e:#}"))
        }
    };
    let (Some(version), Some(up_uuid)) = (uploaded.version.clone(), uploaded.skill_uuid.clone())
    else {
        return json_error(serde_json::json!({
            "status": "blocked",
            "reason": uploaded.reason.unwrap_or_else(|| "not_accepted".into()),
            "message": "The registry did not accept the upload.",
        }));
    };
    if up_uuid != skill_uuid {
        return ToolCallResult::error_result(
            "The registry matched this folder to a different skill than Scenery recorded. \
             Nothing was shared.",
        );
    }
    note_uploaded(state, &dir, &check.tree_hash);

    let confirm = mint_token(PendingShare {
        slug: slug.clone(),
        skill_uuid,
        tree_hash: check.tree_hash.clone(),
        catalog_name: catalog_name.clone(),
        version: version.clone(),
        groups: groups.clone(),
        expires: Instant::now() + TOKEN_TTL,
    });
    let audience = if groups.is_empty() {
        "everyone in your organization".to_string()
    } else {
        format!("members of {} group(s) you belong to", groups.len())
    };
    json_result(serde_json::json!({
        "status": "awaiting_confirmation",
        "preview": {
            "catalog_name": catalog_name,
            "audience": audience,
            "version": version,
            "scan_verdict": preview.verdict,
            "scan_findings": preview.findings,
            "it_review_required": true,
            "what_happens": "It goes to IT for review first and stays private to you until they \
                             approve it. A share made from an AI client is never published \
                             straight to the catalog.",
            "your_copy": "Your own private copy stays yours and is not changed.",
        },
        "confirm_token": confirm,
        "next": "Show this preview to the user in plain words (it will go to IT for review) \
                 and ask if they want to share. ONLY if they say yes, call scenery_share_skill again with the same name \
                 and this confirm_token. The token works once and only for this exact content.",
    }))
}

fn finish_share(client: &RegistryClient, p: &PendingShare) -> ToolCallResult {
    // `force_review` always: see the module docs.
    match client.share_private_skill(&p.skill_uuid, &p.catalog_name, &p.version, &p.groups, true) {
        Ok(AuthoringReply::Done(s)) => {
            let live = s.visibility == "org_published";
            json_result(serde_json::json!({
                "status": if live { "shared" } else { "pending_review" },
                "catalog_name": s.shared_skill_id,
                "message": if live {
                    "Shared. It is now in the company catalog."
                } else {
                    "Submitted. IT will review it; it stays private to the user until approved."
                },
            }))
        }
        Ok(AuthoringReply::Refused(r)) => match r.code.as_str() {
            "name_clash" => json_result(serde_json::json!({
                "status": "name_clash",
                "suggestions": r.suggestions,
                "message": "That catalog name is taken. Ask the user for another and run the \
                            first step again with catalog_name.",
            })),
            "name_clash_branch" => json_error(serde_json::json!({
                "status": "blocked",
                "reason": "name_clash_branch",
                "message": r.message,
            })),
            _ => refusal("share", &r),
        },
        Err(e) => ToolCallResult::error_result(format!("Could not reach the registry: {e:#}")),
    }
}

fn refusal(what: &str, r: &scenery_core::registry::AuthoringRefusal) -> ToolCallResult {
    let hint = match r.status {
        426 => " Your Scenery runner is too old for this; run `brew upgrade scenery`.",
        401 => " Sign in again with scenery_login.",
        _ => "",
    };
    json_error(serde_json::json!({
        "status": "blocked",
        "step": what,
        "http_status": r.status,
        "reason": r.code,
        "files": r.files,
        "message": format!("{}{hint}", if r.message.is_empty() { &r.code } else { &r.message }),
    }))
}

/// After a successful upload the registry holds these exact bytes, so the
/// recorded "what Scenery last wrote" hash moves up to it: an uninstall right
/// after sharing then has no unuploaded edits to back up. Best effort.
fn note_uploaded(state: &AppState, dir: &Path, tree_hash: &str) {
    if let Ok(conn) = Connection::open(&state.db_path) {
        let _ = conn.execute(
            "UPDATE managed_path_markers SET tree_hash = ?1 \
             WHERE path = ?2 AND kind = 'skill' AND origin = 'auto_managed'",
            rusqlite::params![tree_hash, dir.to_string_lossy().to_string()],
        );
    }
}

#[cfg(test)]
#[path = "authoring_tools_tests.rs"]
mod tests;
