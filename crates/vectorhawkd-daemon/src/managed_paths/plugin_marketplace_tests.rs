//! Tests for the plugin marketplace pusher. They write into a temp HOME and
//! assert the on-disk state matches the verified `claude plugin install` layout.
#![allow(clippy::unwrap_used)]

use super::*;

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
    }
}

#[test]
fn install_plugin_bundle_writes_full_state() {
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
fn install_then_uninstall_is_clean_and_preserves_other_marketplaces() {
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
