//! Tests for the plugin marketplace pusher. They write into a temp HOME and
//! assert the on-disk state matches the verified `claude plugin install` layout.
#![allow(clippy::unwrap_used)]

use super::*;
use crate::managed_paths::ENV_MUTEX;

fn bundle() -> PluginBundle {
    PluginBundle {
        slug: "superpowers".to_string(),
        name: "superpowers".to_string(),
        description: "Core skills library".to_string(),
        version: "5.0.7".to_string(),
        author: "Jesse Vincent".to_string(),
        skills: vec![BundledSkill {
            skill_id: "tdd".to_string(),
            skill_md: b"---\nname: tdd\n---\nWrite tests first.".to_vec(),
            files: vec![("prompts/x.md".to_string(), b"p".to_vec())],
        }],
        files: vec![],
    }
}

#[test]
fn install_plugin_bundle_writes_full_state() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let prev = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let result = install_plugin_bundle(&bundle());

    let h = fake_home.path();
    let mp = h.join(".claude/plugins/marketplaces/vectorhawk");
    let plugin_json = mp.join("plugins/superpowers/.claude-plugin/plugin.json");
    let skill_md = mp.join("plugins/superpowers/skills/tdd/SKILL.md");
    let mkt = mp.join(".claude-plugin/marketplace.json");
    let cache =
        h.join(".claude/plugins/cache/vectorhawk/superpowers/5.0.7/.claude-plugin/plugin.json");
    let known = h.join(".claude/plugins/known_marketplaces.json");
    let installed = h.join(".claude/plugins/installed_plugins.json");
    let settings = h.join(".claude/settings.json");

    let read = |p: &std::path::Path| {
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(p).unwrap()).unwrap()
    };

    let plugin_json_ok = plugin_json.exists() && read(&plugin_json)["name"] == "superpowers";
    let skill_ok = std::fs::read(&skill_md).unwrap() == b"---\nname: tdd\n---\nWrite tests first.";
    let bundled_skill_prompt = mp
        .join("plugins/superpowers/skills/tdd/prompts/x.md")
        .exists();
    let mkt_lists = read(&mkt)["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "superpowers");
    let cache_ok = cache.exists();
    let known_ok = read(&known)["vectorhawk"]["source"]["source"] == "directory";
    let installed_ok = read(&installed)["plugins"]["superpowers@vectorhawk"][0]["scope"] == "user"
        && read(&installed)["version"] == 2;
    let enabled_ok = read(&settings)["enabledPlugins"]["superpowers@vectorhawk"] == true;
    let extra_mkt_ok =
        read(&settings)["extraKnownMarketplaces"]["vectorhawk"]["source"]["source"] == "directory";

    if let Some(v) = prev.clone() {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert!(result.is_ok(), "install must succeed");
    assert!(plugin_json_ok, "plugin.json with name");
    assert!(skill_ok, "skill SKILL.md bundled");
    assert!(bundled_skill_prompt, "skill sibling file bundled");
    assert!(mkt_lists, "marketplace.json lists the plugin");
    assert!(cache_ok, "plugin copied into cache");
    assert!(known_ok, "known_marketplaces has vectorhawk dir source");
    assert!(
        installed_ok,
        "installed_plugins v2 has the plugin under user scope"
    );
    assert!(enabled_ok, "settings enables the plugin");
    assert!(extra_mkt_ok, "settings declares the marketplace");
}

#[test]
fn install_plugin_bundle_writes_imported_file_tree_verbatim() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let prev = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());

    let b = PluginBundle {
        slug: "caveman".to_string(),
        name: "caveman".to_string(),
        description: "Talk like caveman".to_string(),
        version: "0.1.0".to_string(),
        author: "Julius Brussee".to_string(),
        skills: vec![],
        files: vec![
            (
                ".claude-plugin/plugin.json".to_string(),
                br#"{"name":"caveman","description":"x"}"#.to_vec(),
            ),
            (
                "commands/caveman.toml".to_string(),
                b"prompt = 'ugh'".to_vec(),
            ),
            (
                "agents/cavecrew-builder.md".to_string(),
                b"---\nname: cavecrew-builder\n---\nbuild".to_vec(),
            ),
            (
                "skills/caveman/SKILL.md".to_string(),
                b"---\nname: caveman\n---\ncompress".to_vec(),
            ),
            // Path-traversal attempt must be ignored.
            ("../evil.sh".to_string(), b"rm -rf".to_vec()),
        ],
    };

    let result = install_plugin_bundle(&b);

    let h = fake_home.path();
    let src = h.join(".claude/plugins/marketplaces/vectorhawk/plugins/caveman");
    let pj_ok = src.join(".claude-plugin/plugin.json").exists();
    let cmd_ok = std::fs::read(src.join("commands/caveman.toml")).unwrap() == b"prompt = 'ugh'";
    let agent_ok = src.join("agents/cavecrew-builder.md").exists();
    let skill_ok = src.join("skills/caveman/SKILL.md").exists();
    let cache_ok = h
        .join(".claude/plugins/cache/vectorhawk/caveman/0.1.0/commands/caveman.toml")
        .exists();
    let traversal_blocked = !h
        .join(".claude/plugins/marketplaces/vectorhawk/evil.sh")
        .exists()
        && !h.join(".claude/plugins/marketplaces/evil.sh").exists();

    if let Some(v) = prev {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert!(result.is_ok(), "install must succeed");
    assert!(pj_ok, "mirrored plugin.json written");
    assert!(cmd_ok, "command file written verbatim");
    assert!(agent_ok, "agent file written");
    assert!(skill_ok, "skill file written");
    assert!(cache_ok, "tree copied into cache");
    assert!(traversal_blocked, "path traversal entry must be skipped");
}

#[test]
fn install_then_uninstall_is_clean_and_preserves_other_marketplaces() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = tempfile::tempdir().unwrap();
    let prev = std::env::var_os("HOME");
    std::env::set_var("HOME", fake_home.path());
    let h = fake_home.path();

    // Seed a pre-existing foreign marketplace + enabled plugin that must survive.
    let known = h.join(".claude/plugins/known_marketplaces.json");
    std::fs::create_dir_all(known.parent().unwrap()).unwrap();
    std::fs::write(
        &known,
        r#"{"official":{"source":{"source":"github","repo":"a/b"}}}"#,
    )
    .unwrap();
    let settings = h.join(".claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(
        &settings,
        r#"{"enabledPlugins":{"other@official":true},"theme":"dark"}"#,
    )
    .unwrap();

    install_plugin_bundle(&bundle()).unwrap();
    uninstall_plugin_bundle("superpowers").unwrap();

    let read = |p: &std::path::Path| {
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(p).unwrap()).unwrap()
    };
    let plugin_dir_gone = !h
        .join(".claude/plugins/marketplaces/vectorhawk/plugins/superpowers")
        .exists();
    let cache_gone = !h
        .join(".claude/plugins/cache/vectorhawk/superpowers")
        .exists();
    let s = read(&settings);
    let enabled_removed = s["enabledPlugins"].get("superpowers@vectorhawk").is_none();
    let foreign_enabled_kept = s["enabledPlugins"]["other@official"] == true;
    let theme_kept = s["theme"] == "dark";
    let foreign_mkt_kept = read(&known)["official"]["source"]["source"] == "github";

    if let Some(v) = prev {
        std::env::set_var("HOME", v);
    } else {
        std::env::remove_var("HOME");
    }

    assert!(plugin_dir_gone, "plugin source dir removed");
    assert!(cache_gone, "cache removed");
    assert!(enabled_removed, "enabledPlugins entry removed");
    assert!(foreign_enabled_kept, "foreign enabled plugin preserved");
    assert!(theme_kept, "unrelated settings preserved");
    assert!(foreign_mkt_kept, "foreign marketplace preserved");
}
