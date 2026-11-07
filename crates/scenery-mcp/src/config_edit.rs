//! Format-preserving edits of AI-client MCP config text.
//!
//! Every function takes the file's text and returns new text, so callers own
//! all I/O. Edits touch only the targeted span: `jsonc-parser`'s concrete
//! syntax tree for JSON and JSONC (comments, trailing commas, indentation and
//! number spellings survive), `toml_edit` for Codex's TOML.
//!
//! Nothing here ever falls back to an empty document. Text that does not
//! parse is an `Err`, and callers refuse to touch that client.

use crate::setup::ConfigFormat;
use anyhow::{anyhow, bail, Context, Result};
use jsonc_parser::cst::{CstInputValue, CstRootNode};
use jsonc_parser::ParseOptions;
use serde_json::Value;

/// Result of [`set_entry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetOutcome {
    /// The edited document text.
    pub text: String,
    /// `true` when the container (`mcpServers`, `servers`, `mcp_servers`) did
    /// not exist before and was created by this edit.
    pub container_created: bool,
}

/// Insert or replace `<container>.<name>` with `value`.
///
/// Blank `text` is treated as a new, empty document. An entry whose value is
/// already equal to `value` is left byte-for-byte untouched.
pub fn set_entry(
    format: ConfigFormat,
    text: &str,
    container: &str,
    name: &str,
    value: &Value,
) -> Result<SetOutcome> {
    match format {
        ConfigFormat::Json => json_set(text, container, name, value),
        ConfigFormat::Toml => toml_set(text, container, name, value),
    }
}

/// Remove `<container>.<name>`. `Ok(None)` when it is not there (nothing to
/// change); `Ok(Some(text))` is the edited document.
pub fn remove_entry(
    format: ConfigFormat,
    text: &str,
    container: &str,
    name: &str,
) -> Result<Option<String>> {
    match format {
        ConfigFormat::Json => json_remove(text, container, name),
        ConfigFormat::Toml => toml_remove(text, container, name),
    }
}

/// Names of the entries under `container`, in file order. Empty when the
/// container is absent or not a table/object.
pub fn entry_names(format: ConfigFormat, text: &str, container: &str) -> Result<Vec<String>> {
    match format {
        ConfigFormat::Json => json_names(text, container),
        ConfigFormat::Toml => toml_names(text, container),
    }
}

/// Drop `container` when it holds no entries. Returns the (possibly
/// unchanged) text.
pub fn prune_empty_container(format: ConfigFormat, text: &str, container: &str) -> Result<String> {
    match format {
        ConfigFormat::Json => json_prune(text, container),
        ConfigFormat::Toml => toml_prune(text, container),
    }
}

/// `true` when the document holds nothing but an empty `container`: no other
/// keys and no comments. Used to decide whether a file Scenery created has
/// anything of the user's in it.
pub fn is_empty_config(format: ConfigFormat, text: &str, container: &str) -> Result<bool> {
    match format {
        ConfigFormat::Json => json_is_empty(text, container),
        ConfigFormat::Toml => toml_is_empty(text, container),
    }
}

// ── JSON / JSONC ─────────────────────────────────────────────────────────────

fn json_root(text: &str) -> Result<CstRootNode> {
    CstRootNode::parse(text, &ParseOptions::default())
        .map_err(|e| anyhow!("not valid JSON or JSONC: {e}"))
}

fn json_input(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(json_input).collect()),
        Value::Object(map) => CstInputValue::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), json_input(v)))
                .collect(),
        ),
    }
}

fn json_set(text: &str, container: &str, name: &str, value: &Value) -> Result<SetOutcome> {
    let source = if text.trim().is_empty() { "{}\n" } else { text };
    let root = json_root(source)?;
    let root_obj = root
        .object_value_or_create()
        .ok_or_else(|| anyhow!("the top-level value is not an object"))?;

    let container_created = root_obj.get(container).is_none();
    let servers = if container_created {
        root_obj.object_value_or_set(container)
    } else {
        root_obj
            .object_value(container)
            .ok_or_else(|| anyhow!("`{container}` is present but is not an object"))?
    };

    match servers.get(name) {
        Some(prop) => {
            let unchanged = prop
                .value()
                .and_then(|v| v.to_serde_value())
                .is_some_and(|existing| &existing == value);
            if unchanged {
                return Ok(SetOutcome {
                    text: text.to_string(),
                    container_created: false,
                });
            }
            prop.set_value(json_input(value));
        }
        None => {
            servers.append(name, json_input(value));
        }
    }
    Ok(SetOutcome {
        text: root.to_string(),
        container_created,
    })
}

fn json_remove(text: &str, container: &str, name: &str) -> Result<Option<String>> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    let root = json_root(text)?;
    let Some(root_obj) = root.object_value() else {
        return Ok(None);
    };
    let Some(servers) = root_obj.object_value(container) else {
        return Ok(None);
    };
    let Some(prop) = servers.get(name) else {
        return Ok(None);
    };
    prop.remove();
    Ok(Some(root.to_string()))
}

fn json_names(text: &str, container: &str) -> Result<Vec<String>> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let root = json_root(text)?;
    let Some(servers) = root
        .object_value()
        .and_then(|obj| obj.object_value(container))
    else {
        return Ok(Vec::new());
    };
    Ok(servers
        .properties()
        .iter()
        .filter_map(|p| p.decoded_name())
        .collect())
}

fn json_prune(text: &str, container: &str) -> Result<String> {
    let root = json_root(text)?;
    let Some(root_obj) = root.object_value() else {
        return Ok(text.to_string());
    };
    let Some(servers) = root_obj.object_value(container) else {
        return Ok(text.to_string());
    };
    if !servers.properties().is_empty() {
        return Ok(text.to_string());
    }
    if let Some(prop) = root_obj.get(container) {
        prop.remove();
    }
    Ok(root.to_string())
}

fn json_is_empty(text: &str, container: &str) -> Result<bool> {
    if text.trim().is_empty() {
        return Ok(true);
    }
    // A comment is something a person wrote.
    if has_json_comment(text) {
        return Ok(false);
    }
    let root = json_root(text)?;
    let Some(root_obj) = root.object_value() else {
        return Ok(false);
    };
    for prop in root_obj.properties() {
        if prop.decoded_name().as_deref() != Some(container) {
            return Ok(false);
        }
        match prop.value().and_then(|v| v.as_object()) {
            Some(obj) if obj.properties().is_empty() => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// `true` when `text` has a `//` or `/* */` comment outside a string literal.
fn has_json_comment(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            match b {
                b'\\' => i += 1,
                b'"' => in_string = false,
                _ => {}
            }
        } else if b == b'"' {
            in_string = true;
        } else if b == b'/' && matches!(bytes.get(i + 1), Some(b'/') | Some(b'*')) {
            return true;
        }
        i += 1;
    }
    false
}

// ── TOML ─────────────────────────────────────────────────────────────────────

fn toml_doc(text: &str) -> Result<toml_edit::DocumentMut> {
    if text.trim().is_empty() {
        return Ok(toml_edit::DocumentMut::new());
    }
    text.parse::<toml_edit::DocumentMut>()
        .map_err(|e| anyhow!("not valid TOML: {e}"))
}

fn toml_value(value: &Value) -> Result<toml_edit::Value> {
    use toml_edit::{Array, InlineTable};
    Ok(match value {
        Value::Null => bail!("TOML has no null value"),
        Value::Bool(b) => (*b).into(),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => i.into(),
            (None, Some(f)) => f.into(),
            (None, None) => bail!("number {n} does not fit a TOML number"),
        },
        Value::String(s) => s.as_str().into(),
        Value::Array(items) => {
            let mut array = Array::new();
            for item in items {
                array.push(toml_value(item)?);
            }
            array.into()
        }
        Value::Object(map) => {
            let mut table = InlineTable::new();
            for (k, v) in map {
                table.insert(k, toml_value(v)?);
            }
            table.into()
        }
    })
}

fn toml_entry_table(value: &Value) -> Result<toml_edit::Table> {
    let Value::Object(map) = value else {
        bail!("an MCP entry must be an object");
    };
    let mut table = toml_edit::Table::new();
    for (k, v) in map {
        table[k.as_str()] = toml_edit::Item::Value(toml_value(v)?);
    }
    Ok(table)
}

fn toml_set(text: &str, container: &str, name: &str, value: &Value) -> Result<SetOutcome> {
    let mut doc = toml_doc(text)?;
    let container_created = doc.get(container).is_none();
    if container_created {
        doc[container] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    // `as_table_like_mut` so an inline `mcp_servers = { ... }` works as well as
    // a `[mcp_servers]` header; only a non-table value is refused.
    let servers = doc[container]
        .as_table_like_mut()
        .ok_or_else(|| anyhow!("`{container}` is present but is not a table"))?;
    servers.insert(name, toml_edit::Item::Table(toml_entry_table(value)?));
    Ok(SetOutcome {
        text: doc.to_string(),
        container_created,
    })
}

fn toml_remove(text: &str, container: &str, name: &str) -> Result<Option<String>> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    let mut doc = toml_doc(text)?;
    let Some(servers) = doc
        .get_mut(container)
        .and_then(|item| item.as_table_like_mut())
    else {
        return Ok(None);
    };
    if servers.remove(name).is_none() {
        return Ok(None);
    }
    Ok(Some(doc.to_string()))
}

fn toml_names(text: &str, container: &str) -> Result<Vec<String>> {
    let doc = toml_doc(text)?;
    Ok(doc
        .get(container)
        .and_then(|item| item.as_table_like())
        .map(|t| t.iter().map(|(k, _)| k.to_string()).collect())
        .unwrap_or_default())
}

fn toml_prune(text: &str, container: &str) -> Result<String> {
    let mut doc = toml_doc(text)?;
    let empty = doc
        .get(container)
        .and_then(|item| item.as_table_like())
        .is_some_and(|t| t.is_empty());
    if !empty {
        return Ok(text.to_string());
    }
    doc.remove(container);
    Ok(doc.to_string())
}

fn toml_is_empty(text: &str, container: &str) -> Result<bool> {
    let doc = toml_doc(text).context("checking whether the remaining TOML is empty")?;
    if text.lines().any(|l| l.trim_start().starts_with('#')) {
        return Ok(false);
    }
    let only_empty_container = doc
        .iter()
        .all(|(key, item)| key == container && item.as_table_like().is_some_and(|t| t.is_empty()));
    Ok(only_empty_container)
}

#[cfg(test)]
#[path = "config_edit_tests.rs"]
mod tests;
