//! Plugin marketplace pusher — makes a governed VectorHawk plugin appear in
//! Claude Code's `/plugin list` as a self-contained bundle.
//!
//! # Why a marketplace?
//!
//! Claude Code does NOT list "loose" plugin directories. A plugin only shows in
//! `/plugin list` (and has its skills/commands loaded) when it comes from a
//! **registered marketplace** and is **enabled**. We therefore maintain a local
//! directory marketplace named `vectorhawk` and register/enable plugins into it
//! by writing the same on-disk state the `claude plugin` CLI produces — verified
//! empirically (see the BUG card). No shell-out to `claude` is required, so this
//! works from the daemon regardless of its PATH.
//!
//! # What we write (all under `~/.claude`)
//!
//! | Path | Purpose |
//! |------|---------|
//! | `plugins/marketplaces/vectorhawk/.claude-plugin/marketplace.json` | catalog listing every VH plugin |
//! | `plugins/marketplaces/vectorhawk/plugins/<slug>/.claude-plugin/plugin.json` | per-plugin manifest |
//! | `plugins/marketplaces/vectorhawk/plugins/<slug>/skills/<id>/SKILL.md` | bundled skill content |
//! | `plugins/cache/vectorhawk/<slug>/<version>/` | install cache (copy of the plugin dir) |
//! | `plugins/known_marketplaces.json` | marketplace registry (merge `vectorhawk`) |
//! | `plugins/installed_plugins.json` | installed-plugin registry v2 (merge `<slug>@vectorhawk`) |
//! | `settings.json` | merge `extraKnownMarketplaces.vectorhawk` + `enabledPlugins["<slug>@vectorhawk"]` |
//!
//! All writes honour the `VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER` killswitch.

use anyhow::{Context, Result};
use fs2::FileExt;
use serde_json::{json, Map, Value};
use std::{fs, path::Path, path::PathBuf};
use tracing::{debug, info};

/// Marketplace name (the `@<marketplace>` suffix Claude Code uses).
pub const MARKETPLACE_NAME: &str = "vectorhawk";

/// A skill to bundle inside a plugin: its id plus the raw SKILL.md and any
/// sibling files (prompts/, etc.).
#[derive(Debug, Clone)]
pub struct BundledSkill {
    pub skill_id: String,
    pub skill_md: Vec<u8>,
    pub files: Vec<(String, Vec<u8>)>,
}

/// Everything needed to materialize a governed plugin into the local
/// marketplace and register/enable it in Claude Code.
#[derive(Debug, Clone)]
pub struct PluginBundle {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub author: String,
    pub skills: Vec<BundledSkill>,
}

fn reconciler_disabled() -> bool {
    std::env::var_os("VECTORHAWK_DISABLE_FILESYSTEM_RECONCILER").is_some()
}

fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow::anyhow!("plugin_marketplace: HOME not resolvable"))
}

fn plugins_dir(home: &Path) -> PathBuf {
    home.join(".claude").join("plugins")
}

/// Source dir of the local `vectorhawk` directory marketplace.
fn marketplace_dir(home: &Path) -> PathBuf {
    plugins_dir(home)
        .join("marketplaces")
        .join(MARKETPLACE_NAME)
}

/// Plugin key as Claude Code addresses it: `<slug>@vectorhawk`.
fn plugin_key(slug: &str) -> String {
    format!("{slug}@{MARKETPLACE_NAME}")
}

/// Install (or refresh) a governed plugin so it appears enabled in
/// `/plugin list`, bundling its skills. Idempotent.
pub fn install_plugin_bundle(bundle: &PluginBundle) -> Result<()> {
    if reconciler_disabled() {
        return Ok(());
    }
    let home = home()?;
    let mp = marketplace_dir(&home);
    let plugin_src = mp.join("plugins").join(&bundle.slug);

    // 1. Write the per-plugin dir in the marketplace source (plugin.json + skills).
    write_plugin_source(&plugin_src, bundle)?;

    // 2. Refresh marketplace.json so the catalog lists this plugin.
    upsert_marketplace_manifest(&mp, bundle)?;

    // 3. Copy the plugin dir into the install cache.
    let cache = plugins_dir(&home)
        .join("cache")
        .join(MARKETPLACE_NAME)
        .join(&bundle.slug)
        .join(&bundle.version);
    if cache.exists() {
        fs::remove_dir_all(&cache).ok();
    }
    copy_dir_recursive(&plugin_src, &cache)
        .with_context(|| format!("plugin_marketplace: failed to populate cache {cache:?}"))?;

    // 4. Register the marketplace + record the install + enable the plugin.
    let now = chrono::Utc::now().to_rfc3339();
    upsert_known_marketplaces(&home, &mp, &now)?;
    upsert_installed_plugins(&home, &bundle.slug, &bundle.version, &cache, &now)?;
    upsert_settings(&home, &mp, &bundle.slug)?;

    info!(
        slug = %bundle.slug,
        skills = bundle.skills.len(),
        "plugin_marketplace: installed plugin into vectorhawk marketplace"
    );
    Ok(())
}

/// Remove a governed plugin from the marketplace, cache, and Claude Code
/// registries. Idempotent.
pub fn uninstall_plugin_bundle(slug: &str) -> Result<()> {
    if reconciler_disabled() {
        return Ok(());
    }
    let home = home()?;
    let mp = marketplace_dir(&home);
    let key = plugin_key(slug);

    fs::remove_dir_all(mp.join("plugins").join(slug)).ok();
    fs::remove_dir_all(
        plugins_dir(&home)
            .join("cache")
            .join(MARKETPLACE_NAME)
            .join(slug),
    )
    .ok();
    remove_from_marketplace_manifest(&mp, slug)?;

    // installed_plugins.json: drop the key.
    edit_json(&plugins_dir(&home).join("installed_plugins.json"), |root| {
        if let Some(plugins) = root.get_mut("plugins").and_then(Value::as_object_mut) {
            plugins.remove(&key);
        }
    })?;
    // settings.json: drop the enabledPlugins entry (leave the marketplace registered).
    edit_json(&home.join(".claude").join("settings.json"), |root| {
        if let Some(enabled) = root
            .get_mut("enabledPlugins")
            .and_then(Value::as_object_mut)
        {
            enabled.remove(&key);
        }
    })?;

    info!(
        slug,
        "plugin_marketplace: uninstalled plugin from vectorhawk marketplace"
    );
    Ok(())
}

// ── Marketplace source dir ──────────────────────────────────────────────────

fn write_plugin_source(plugin_src: &Path, bundle: &PluginBundle) -> Result<()> {
    // Start clean so removed skills don't linger.
    if plugin_src.exists() {
        fs::remove_dir_all(plugin_src).ok();
    }
    let claude_plugin = plugin_src.join(".claude-plugin");
    fs::create_dir_all(&claude_plugin)
        .with_context(|| format!("plugin_marketplace: mkdir {claude_plugin:?}"))?;

    let manifest = json!({
        "name": bundle.slug,
        "version": bundle.version,
        "description": bundle.description,
        "author": { "name": bundle.author },
    });
    atomic_write(
        &claude_plugin.join("plugin.json"),
        serde_json::to_vec_pretty(&manifest)?.as_slice(),
    )?;

    for skill in &bundle.skills {
        let skill_dir = plugin_src.join("skills").join(&skill.skill_id);
        fs::create_dir_all(&skill_dir)
            .with_context(|| format!("plugin_marketplace: mkdir {skill_dir:?}"))?;
        atomic_write(&skill_dir.join("SKILL.md"), &skill.skill_md)?;
        for (rel, bytes) in &skill.files {
            let dest = skill_dir.join(rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).ok();
            }
            atomic_write(&dest, bytes)?;
        }
    }
    Ok(())
}

fn upsert_marketplace_manifest(mp: &Path, bundle: &PluginBundle) -> Result<()> {
    let claude_plugin = mp.join(".claude-plugin");
    fs::create_dir_all(&claude_plugin)
        .with_context(|| format!("plugin_marketplace: mkdir {claude_plugin:?}"))?;
    let path = claude_plugin.join("marketplace.json");

    let mut root = read_json(&path).unwrap_or_else(|| {
        json!({
            "name": MARKETPLACE_NAME,
            "description": "VectorHawk governed plugins",
            "owner": { "name": "VectorHawk" },
            "plugins": [],
        })
    });
    let entry = json!({
        "name": bundle.slug,
        "description": bundle.description,
        "author": { "name": bundle.author },
        "source": format!("./plugins/{}", bundle.slug),
    });
    let arr = root.as_object_mut().and_then(|m| {
        m.entry("plugins")
            .or_insert_with(|| json!([]))
            .as_array_mut()
    });
    if let Some(arr) = arr {
        arr.retain(|p| p.get("name").and_then(Value::as_str) != Some(bundle.slug.as_str()));
        arr.push(entry);
    }
    atomic_write(&path, serde_json::to_vec_pretty(&root)?.as_slice())
}

fn remove_from_marketplace_manifest(mp: &Path, slug: &str) -> Result<()> {
    let path = mp.join(".claude-plugin").join("marketplace.json");
    if !path.exists() {
        return Ok(());
    }
    edit_json(&path, |root| {
        if let Some(arr) = root.get_mut("plugins").and_then(Value::as_array_mut) {
            arr.retain(|p| p.get("name").and_then(Value::as_str) != Some(slug));
        }
    })
}

// ── Claude Code registries ──────────────────────────────────────────────────

fn upsert_known_marketplaces(home: &Path, mp: &Path, now: &str) -> Result<()> {
    let path = plugins_dir(home).join("known_marketplaces.json");
    let mp_str = mp.to_string_lossy().to_string();
    edit_json_create(&path, |root| {
        let obj = ensure_object(root);
        obj.insert(
            MARKETPLACE_NAME.to_string(),
            json!({
                "source": { "source": "directory", "path": mp_str },
                "installLocation": mp_str,
                "lastUpdated": now,
            }),
        );
    })
}

fn upsert_installed_plugins(
    home: &Path,
    slug: &str,
    version: &str,
    cache: &Path,
    now: &str,
) -> Result<()> {
    let path = plugins_dir(home).join("installed_plugins.json");
    let key = plugin_key(slug);
    let install_path = cache.to_string_lossy().to_string();
    edit_json_create(&path, |root| {
        let obj = ensure_object(root);
        obj.entry("version").or_insert_with(|| json!(2));
        let plugins = obj
            .entry("plugins")
            .or_insert_with(|| json!({}))
            .as_object_mut();
        if let Some(plugins) = plugins {
            plugins.insert(
                key.clone(),
                json!([{
                    "scope": "user",
                    "installPath": install_path,
                    "version": version,
                    "installedAt": now,
                    "lastUpdated": now,
                }]),
            );
        }
    })
}

fn upsert_settings(home: &Path, mp: &Path, slug: &str) -> Result<()> {
    let path = home.join(".claude").join("settings.json");
    let mp_str = mp.to_string_lossy().to_string();
    let key = plugin_key(slug);
    edit_json_create(&path, |root| {
        let obj = ensure_object(root);
        let mkts = obj
            .entry("extraKnownMarketplaces")
            .or_insert_with(|| json!({}));
        if let Some(mkts) = mkts.as_object_mut() {
            mkts.insert(
                MARKETPLACE_NAME.to_string(),
                json!({ "source": { "source": "directory", "path": mp_str } }),
            );
        }
        let enabled = obj.entry("enabledPlugins").or_insert_with(|| json!({}));
        if let Some(enabled) = enabled.as_object_mut() {
            enabled.insert(key.clone(), json!(true));
        }
    })
}

// ── JSON helpers (lock + read-modify-write) ─────────────────────────────────

fn ensure_object(root: &mut Value) -> &mut Map<String, Value> {
    if !root.is_object() {
        *root = Value::Object(Map::new());
    }
    root.as_object_mut().expect("just ensured object")
}

fn read_json(path: &Path) -> Option<Value> {
    let s = fs::read_to_string(path).ok()?;
    if s.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&s).ok()
}

/// Edit an existing JSON file in place; no-op if it doesn't exist.
fn edit_json<F: FnOnce(&mut Value)>(path: &Path, mutate: F) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    edit_json_create(path, mutate)
}

/// Edit a JSON file under an exclusive lock, creating it (and parents) if
/// absent. A missing/empty/invalid file starts as `{}`.
fn edit_json_create<F: FnOnce(&mut Value)>(path: &Path, mutate: F) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("plugin_marketplace: no parent for {path:?}"))?;
    fs::create_dir_all(parent).with_context(|| format!("plugin_marketplace: mkdir {parent:?}"))?;

    let lock_path = path.with_extension("lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("plugin_marketplace: open lock {lock_path:?}"))?;
    lock.lock_exclusive()
        .with_context(|| format!("plugin_marketplace: lock {lock_path:?}"))?;

    let mut root = read_json(path).unwrap_or_else(|| Value::Object(Map::new()));
    mutate(&mut root);
    let bytes = serde_json::to_vec_pretty(&root)?;
    atomic_write(path, &bytes)?;

    drop(lock);
    Ok(())
}

fn atomic_write(dest: &Path, content: &[u8]) -> Result<()> {
    let tmp = dest.with_extension("tmp");
    fs::write(&tmp, content).with_context(|| format!("plugin_marketplace: write {tmp:?}"))?;
    fs::rename(&tmp, dest).with_context(|| format!("plugin_marketplace: rename -> {dest:?}"))?;
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            if let Some(p) = to.parent() {
                fs::create_dir_all(p).ok();
            }
            fs::copy(&from, &to)?;
            debug!(?to, "plugin_marketplace: cached file");
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "plugin_marketplace_tests.rs"]
mod tests;
