//! Human-readable scan summary.

use scenery_daemon::managed_paths::discoveries::DiscoveryItem;

const MAX_ROWS: usize = 50;

fn detail_str<'a>(item: &'a DiscoveryItem, key: &str) -> Option<&'a str> {
    item.detail.as_ref()?.get(key)?.as_str()
}

fn flag(item: &DiscoveryItem) -> String {
    if let Some(reason) = detail_str(item, "hold_reason") {
        return format!("hold:{reason}");
    }
    let suspicious = item
        .detail
        .as_ref()
        .and_then(|d| d.get("suspicious"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if suspicious {
        "suspicious".to_string()
    } else {
        String::new()
    }
}

fn name(item: &DiscoveryItem) -> String {
    if item.kind == "mcp" {
        if let Some(key) = detail_str(item, "server_key") {
            return key.to_string();
        }
    }
    item.slug.clone()
}

pub fn render(items: &[DiscoveryItem], quiet: bool) -> String {
    let count = |k: &str| items.iter().filter(|i| i.kind == k).count();
    let mut out = format!(
        "Found {} MCP servers, {} skills, {} plugins.\n",
        count("mcp"),
        count("skill"),
        count("plugin")
    );
    if quiet || items.is_empty() {
        return out;
    }
    let mut rows: Vec<(String, String, String, String)> = items
        .iter()
        .map(|i| {
            let client = if i.kind == "mcp" {
                detail_str(i, "client_name").unwrap_or("").to_string()
            } else {
                String::new()
            };
            (i.kind.clone(), client, name(i), flag(i))
        })
        .collect();
    rows.sort_by(|a, b| (&a.0, &a.2).cmp(&(&b.0, &b.2)));
    let shown = rows.len().min(MAX_ROWS);
    out.push_str(&format!(
        "\n{:<8}{:<16}{:<40}{}\n",
        "KIND", "CLIENT", "NAME", "FLAG"
    ));
    for (kind, client, name, flag) in rows.iter().take(shown) {
        out.push_str(&format!("{kind:<8}{client:<16}{name:<40}{flag}\n"));
    }
    if rows.len() > shown {
        out.push_str(&format!("… and {} more\n", rows.len() - shown));
    }
    out
}
