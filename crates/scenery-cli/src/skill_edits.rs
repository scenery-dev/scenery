//! `scenery skill merge|share|history|rollback` (AUTO-MANAGE Phase 9b).
//!
//! Thin CLI over `scenery_daemon::managed_paths::skill_merge` and the runner
//! registry endpoints; the logic lives there.

use anyhow::{bail, Context, Result};
use scenery_core::{
    registry::{AuthoringReply, RegistryClient},
    state::AppState,
};
use scenery_daemon::managed_paths::{
    local_edits,
    skill_merge::{self, MergeOutcome},
};
use std::sync::Arc;

fn state() -> Result<Arc<AppState>> {
    Ok(Arc::new(
        AppState::bootstrap().context("failed to bootstrap state")?,
    ))
}

fn url(registry_url: Option<&str>) -> String {
    registry_url
        .unwrap_or(scenery_core::DEFAULT_REGISTRY_URL)
        .to_string()
}

/// `scenery skill merge <slug> [--continue] [--apply]`.
pub async fn cmd_skill_merge(
    slug: &str,
    continue_: bool,
    apply: bool,
    registry_url: Option<&str>,
) -> Result<()> {
    let state = state()?;
    let registry = url(registry_url);
    if continue_ || apply {
        let out = skill_merge::continue_merge(&state, &registry, slug, apply).await?;
        println!(
            "Uploaded your merged '{slug}' as {}.",
            out.version.as_deref().unwrap_or("a new version")
        );
        if let Some(b) = &out.applied_backup {
            println!(
                "Applied to the live skill. The tree it replaced is kept at {}.",
                b.display()
            );
        } else if let Some(why) = &out.not_applied_reason {
            println!("Not applied: {why}.");
        } else {
            println!(
                "The live skill is unchanged. Run `scenery skill merge {slug} --apply` to switch \
                 it to the merged tree."
            );
        }
        println!("You can now submit it: `scenery skill share {slug}`.");
        return Ok(());
    }
    match skill_merge::prepare_merge(&state, &registry, slug).await? {
        MergeOutcome::UpToDate { head_version } => {
            println!(
                "'{slug}' is already based on the latest catalog version{}. Nothing to merge.",
                head_version.map(|v| format!(" ({v})")).unwrap_or_default()
            );
        }
        MergeOutcome::Written {
            dir,
            head_version,
            conflicts,
        } => {
            println!(
                "Merged the catalog update{} into your edits at {}.",
                head_version.map(|v| format!(" ({v})")).unwrap_or_default(),
                dir.display()
            );
            println!("Your running skill was not touched.");
            if conflicts.is_empty() {
                println!("No conflicts. Run `scenery skill merge {slug} --continue` to upload it (add --apply to switch the live skill to it).");
            } else {
                println!("Conflicts in:");
                for c in &conflicts {
                    println!("  {c}");
                }
                println!(
                    "Edit those files (remove the <<<<<<< ======= >>>>>>> lines; for a .theirs \
                     copy, keep the version you want and delete the .theirs file), then run \
                     `scenery skill merge {slug} --continue`."
                );
            }
        }
    }
    Ok(())
}

/// `scenery skill share <slug>`: a catalog branch goes to IT as a change
/// request; a dev or own skill goes through the Share-to-catalog flow.
pub async fn cmd_skill_share(
    slug: &str,
    name: Option<&str>,
    notes: Option<&str>,
    yes: bool,
    registry_url: Option<&str>,
) -> Result<()> {
    let state = state()?;
    let registry = url(registry_url);
    let local = skill_merge::find_local_skill(&state, slug);
    let is_catalog_branch = local.as_ref().is_some_and(|l| {
        l.authorship == local_edits::Authorship::Catalog
            && l.marker.as_ref().map_or(true, |m| {
                rusqlite::Connection::open(&state.db_path)
                    .map(|c| {
                        !scenery_daemon::managed_paths::marker::marker_provenance(&c, &m.path)
                            .is_auto_managed()
                    })
                    .unwrap_or(true)
            })
            && (l.state.branch_state == local_edits::BRANCH_BRANCH || l.marker.is_some())
    });
    if is_catalog_branch {
        let out = skill_merge::share_branch(&state, &registry, slug, notes).await?;
        println!(
            "Submitted {} of '{slug}' for IT review (request {}, {}). It keeps running for you; \
             the catalog changes only if IT approves.",
            out.version, out.request_id, out.status
        );
        return Ok(());
    }
    share_own_or_dev(&state, &registry, slug, name, yes).await
}

fn tool_text(r: &scenery_mcp::protocol::ToolCallResult) -> String {
    r.content
        .first()
        .map(|c| c.text.clone())
        .unwrap_or_default()
}

async fn share_own_or_dev(
    state: &Arc<AppState>,
    registry: &str,
    slug: &str,
    name: Option<&str>,
    yes: bool,
) -> Result<()> {
    let mut args = serde_json::json!({ "name": slug });
    if let Some(n) = name {
        args["catalog_name"] = serde_json::json!(n);
    }
    let call = |args: serde_json::Value| {
        let (st, reg) = (Arc::clone(state), Some(registry.to_string()));
        async move {
            tokio::task::spawn_blocking(move || {
                scenery_mcp::authoring_tools::handle("scenery_share_skill", &args, &st, &reg)
            })
            .await
            .context("share task failed")
        }
    };
    let first = call(args.clone()).await?;
    let v: serde_json::Value = serde_json::from_str(&tool_text(&first)).unwrap_or_default();
    let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
    if status != "awaiting_confirmation" {
        bail!("{}", describe(&v, &tool_text(&first)));
    }
    let p = &v["preview"];
    println!(
        "Share '{slug}' to the catalog as '{}' ({}).",
        p["catalog_name"].as_str().unwrap_or(slug),
        p["audience"].as_str().unwrap_or("your organization")
    );
    println!(
        "Scan: {}. {}",
        p["scan_verdict"].as_str().unwrap_or("unknown"),
        p["what_happens"].as_str().unwrap_or("")
    );
    if !yes && !confirm("Submit for IT review?")? {
        println!("Cancelled. Nothing was shared.");
        return Ok(());
    }
    let token = v["confirm_token"].as_str().unwrap_or_default().to_string();
    args["confirm_token"] = serde_json::json!(token);
    let second = call(args).await?;
    let v2: serde_json::Value = serde_json::from_str(&tool_text(&second)).unwrap_or_default();
    match v2.get("status").and_then(|s| s.as_str()) {
        Some("shared") | Some("pending_review") => {
            println!("{}", v2["message"].as_str().unwrap_or("Submitted."));
            Ok(())
        }
        _ => bail!("{}", describe(&v2, &tool_text(&second))),
    }
}

fn describe(v: &serde_json::Value, raw: &str) -> String {
    let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("error");
    let msg = v
        .get("message")
        .and_then(|m| m.as_str())
        .or_else(|| v.get("reason").and_then(|m| m.as_str()))
        .unwrap_or(raw);
    format!("share: {status}: {msg}")
}

fn confirm(prompt: &str) -> Result<bool> {
    use std::io::{BufRead, Write};
    print!("{prompt} [y/N] ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// (url, token): the blocking client is built inside `spawn_blocking`.
fn client(state: &AppState, registry: &str) -> Result<(String, String)> {
    let token = scenery_core::auth::load_tokens(state, registry)
        .ok()
        .flatten()
        .map(|t| t.access_token)
        .filter(|t| !t.is_empty())
        .context("not signed in; run `scenery auth login`")?;
    Ok((registry.to_string(), token))
}

fn registry_id(state: &AppState, slug: &str) -> String {
    skill_merge::find_local_skill(state, slug)
        .map(|l| l.skill_id)
        .unwrap_or_else(|| slug.to_string())
}

/// `scenery skill history <slug>`.
pub async fn cmd_skill_history(slug: &str, registry_url: Option<&str>) -> Result<()> {
    let state = state()?;
    let registry = url(registry_url);
    let id = registry_id(&state, slug);
    let (u, t) = client(&state, &registry)?;
    let reply =
        tokio::task::spawn_blocking(move || RegistryClient::new(u).with_auth(t).skill_history(&id))
            .await??;
    let h = match reply {
        AuthoringReply::Done(h) => h,
        AuthoringReply::Refused(r) => bail!("history refused ({}): {}", r.status, r.code),
    };
    println!(
        "{} ({}), head {}",
        if h.name.is_empty() { slug } else { &h.name },
        h.authorship,
        h.head_version.as_deref().unwrap_or("-")
    );
    for v in &h.versions {
        let mut tags = Vec::new();
        if v.is_head {
            tags.push("head");
        }
        if !v.installed_on.is_empty() {
            tags.push("installed");
        }
        if !v.is_published {
            tags.push("branch");
        }
        println!(
            "  {:<28} {:<8} {}{}",
            v.version,
            v.lineage,
            tags.join(","),
            v.base_version
                .as_deref()
                .map(|b| format!("  (from {b})"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

/// `scenery skill rollback <slug> <version> [--device <id>]`.
pub async fn cmd_skill_rollback(
    slug: &str,
    version: &str,
    device: Option<&str>,
    registry_url: Option<&str>,
) -> Result<()> {
    let state = state()?;
    let registry = url(registry_url);
    let id = registry_id(&state, slug);
    let (u, t) = client(&state, &registry)?;
    let (v, d) = (version.to_string(), device.map(str::to_string));
    let reply = tokio::task::spawn_blocking(move || {
        RegistryClient::new(u)
            .with_auth(t)
            .skill_rollback(&id, &v, d.as_deref())
    })
    .await??;
    match reply {
        AuthoringReply::Done(r) => {
            println!(
                "Rolled '{slug}' back to {} on {} device(s). The runner installs it on its next sync.",
                r.version,
                r.devices.len()
            );
            Ok(())
        }
        AuthoringReply::Refused(r) => bail!(
            "rollback refused ({}): {}",
            r.status,
            if r.message.is_empty() {
                r.code
            } else {
                r.message
            }
        ),
    }
}
