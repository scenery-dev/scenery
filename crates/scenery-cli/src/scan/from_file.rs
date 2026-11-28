//! `--from-file`: upload a previously saved report. The file stays in place.
//!
//! A saved file may have been edited, or written by another build, so its MCP
//! args go through `redact_args` again before anything is posted.

use super::flow::{print_receipt, runner_present};
use super::report;
use super::upload::{self, Pacing, Uplink, UploadError};
use anyhow::Result;
use scenery_daemon::managed_paths::mcp_args::redact_args;
use serde_json::{Map, Value};
use std::path::Path;

pub(super) async fn run(
    path: &Path,
    link: &Uplink<'_>,
    pacing: &Pacing<'_>,
    out: &mut dyn std::io::Write,
) -> Result<i32> {
    let body = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            writeln!(
                out,
                "scenery scan: could not read {}: {}",
                path.display(),
                e.kind()
            )?;
            return Ok(2);
        }
    };
    let mut value = match serde_json::from_slice::<Value>(&body) {
        Ok(v) if v.get("format").and_then(Value::as_str) == Some(report::REPORT_FORMAT) => v,
        _ => {
            writeln!(
                out,
                "scenery scan: {} is not a scan report.",
                path.display()
            )?;
            return Ok(2);
        }
    };
    redact_items(&mut value);
    let body = serde_json::to_vec(&value)?;
    match upload::post_body(link, body, pacing).await {
        Ok(receipt) => {
            print_receipt("your org", &receipt, out)?;
            Ok(0)
        }
        Err(UploadError::RunnerPresent) => runner_present(out),
        Err(e) => {
            if let Some(line) = e.saved_line(path) {
                writeln!(out, "{line}")?;
                return Ok(3);
            }
            writeln!(
                out,
                "Upload failed: {e}. Report kept at {p} — send it to IT, or retry with: \
                 scenery scan --once --from-file {p}",
                p = path.display()
            )?;
            Ok(3)
        }
    }
}

/// Re-apply `redact_args` to every MCP item's `detail.args`.
pub(super) fn redact_items(report: &mut Value) {
    let Some(items) = report.get_mut("items").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items.iter_mut() {
        if item.get("kind").and_then(Value::as_str) != Some("mcp") {
            continue;
        }
        if let Some(detail) = item.get_mut("detail").and_then(Value::as_object_mut) {
            redact_detail(detail);
        }
    }
}

/// Redact `args` in place. Args that are not all strings, or that have no
/// command to redact against, are dropped (the backend accepts items without).
fn redact_detail(detail: &mut Map<String, Value>) {
    let Some(raw) = detail.get("args") else {
        return;
    };
    let args: Option<Vec<String>> = raw
        .as_array()
        .and_then(|a| a.iter().map(|v| v.as_str().map(str::to_string)).collect());
    let basename = detail
        .get("command_basename")
        .or_else(|| detail.get("command"))
        .and_then(Value::as_str)
        .map(str::to_string);
    match (basename, args) {
        (Some(b), Some(args)) => {
            let redacted = redact_args(&b, &args);
            detail.insert("args".to_string(), Value::from(redacted));
        }
        (None, _) | (_, None) => {
            detail.remove("args");
        }
    }
}
