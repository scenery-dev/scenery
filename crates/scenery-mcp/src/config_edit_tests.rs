#![allow(clippy::unwrap_used)]
use super::*;
use serde_json::json;

fn entry() -> Value {
    json!({"command": "scenery", "args": ["mcp", "serve"]})
}

fn json_roundtrip(input: &str, key: &str) -> String {
    let set = set_entry(ConfigFormat::Json, input, key, "scenery", &entry()).unwrap();
    let back = remove_entry(ConfigFormat::Json, &set.text, key, "scenery")
        .unwrap()
        .unwrap();
    if set.container_created {
        prune_empty_container(ConfigFormat::Json, &back, key).unwrap()
    } else {
        back
    }
}

#[test]
fn four_space_indent_and_odd_key_order_round_trip_byte_identical() {
    let input = "{\n    \"zeta\": 1,\n    \"mcpServers\": {\n        \"b\": {\"command\": \"x\"}\n    },\n    \"alpha\": 2\n}\n";
    assert_eq!(json_roundtrip(input, "mcpServers"), input);
}

#[test]
fn jsonc_comments_and_trailing_commas_survive_both_ways() {
    let input = "{\n  // c\n  \"servers\": {\n    \"b\": {}, // keep\n  },\n}\n";
    let set = set_entry(ConfigFormat::Json, input, "servers", "scenery", &entry()).unwrap();
    assert!(set.text.contains("// c") && set.text.contains("// keep"));
    assert_eq!(json_roundtrip(input, "servers"), input);
}

#[test]
fn big_integers_are_not_coerced() {
    let input = "{\"id\": 12345678901234567890123, \"mcpServers\": {}}";
    let set = set_entry(ConfigFormat::Json, input, "mcpServers", "scenery", &entry()).unwrap();
    assert!(set.text.contains("12345678901234567890123"), "{}", set.text);
}

#[test]
fn blank_and_missing_container_are_created_and_pruned_again() {
    let set = set_entry(ConfigFormat::Json, "", "mcpServers", "scenery", &entry()).unwrap();
    assert!(set.container_created);
    let back = remove_entry(ConfigFormat::Json, &set.text, "mcpServers", "scenery")
        .unwrap()
        .unwrap();
    let pruned = prune_empty_container(ConfigFormat::Json, &back, "mcpServers").unwrap();
    assert!(is_empty_config(ConfigFormat::Json, &pruned, "mcpServers").unwrap());
    assert!(is_empty_config(ConfigFormat::Json, &back, "mcpServers").unwrap());
}

#[test]
fn unparsable_text_is_an_error_never_an_empty_document() {
    for bad in ["{\"a\": ", "[1,2]", "not json"] {
        assert!(set_entry(ConfigFormat::Json, bad, "mcpServers", "scenery", &entry()).is_err());
    }
    assert!(set_entry(
        ConfigFormat::Toml,
        "= = =",
        "mcp_servers",
        "scenery",
        &entry()
    )
    .is_err());
}

#[test]
fn is_empty_config_is_false_when_the_user_added_something() {
    let with_key = "{\"theme\": \"dark\", \"mcpServers\": {}}";
    assert!(!is_empty_config(ConfigFormat::Json, with_key, "mcpServers").unwrap());
    let with_server = "{\"mcpServers\": {\"mine\": {}}}";
    assert!(!is_empty_config(ConfigFormat::Json, with_server, "mcpServers").unwrap());
    let with_comment = "{ // mine\n\"mcpServers\": {}}";
    assert!(!is_empty_config(ConfigFormat::Json, with_comment, "mcpServers").unwrap());
}

#[test]
fn entry_names_reads_jsonc_and_toml() {
    let jsonc = "{ // c\n \"servers\": { \"a\": {}, \"b\": {}, } }";
    assert_eq!(
        entry_names(ConfigFormat::Json, jsonc, "servers").unwrap(),
        ["a", "b"]
    );
    let toml = "[mcp_servers.x]\ncommand = \"1\"\n[mcp_servers.y]\ncommand = \"2\"\n";
    assert_eq!(
        entry_names(ConfigFormat::Toml, toml, "mcp_servers").unwrap(),
        ["x", "y"]
    );
}

#[test]
fn toml_with_comments_round_trips_byte_identical() {
    let input = "# codex\nmodel = \"gpt-5\" # pinned\n\n[mcp_servers.mine]\ncommand = \"x\"\n";
    let set = set_entry(
        ConfigFormat::Toml,
        input,
        "mcp_servers",
        "scenery",
        &entry(),
    )
    .unwrap();
    assert!(!set.container_created);
    let back = remove_entry(ConfigFormat::Toml, &set.text, "mcp_servers", "scenery")
        .unwrap()
        .unwrap();
    assert_eq!(back, input);
}

#[test]
fn toml_created_container_prunes_to_empty() {
    let set = set_entry(ConfigFormat::Toml, "", "mcp_servers", "scenery", &entry()).unwrap();
    assert!(set.container_created);
    let back = remove_entry(ConfigFormat::Toml, &set.text, "mcp_servers", "scenery")
        .unwrap()
        .unwrap();
    assert!(is_empty_config(ConfigFormat::Toml, &back, "mcp_servers").unwrap());
    let pruned = prune_empty_container(ConfigFormat::Toml, &back, "mcp_servers").unwrap();
    assert!(pruned.trim().is_empty(), "{pruned:?}");
}

#[test]
fn remove_of_an_absent_entry_reports_nothing_to_do() {
    assert!(
        remove_entry(ConfigFormat::Json, "{}", "mcpServers", "scenery")
            .unwrap()
            .is_none()
    );
    assert!(
        remove_entry(ConfigFormat::Toml, "a = 1\n", "mcp_servers", "scenery")
            .unwrap()
            .is_none()
    );
}
