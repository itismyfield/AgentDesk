//! Claude Code background sessions (`claude agents --json`), which a resume must not take over.

use std::process::Stdio;
use std::time::Duration;

use crate::services::claude_command::{ClaudeBinary, ClaudeCommandBuilder};

/// A slow or broken CLI must not hold up the resume, so the lookup gives up after this.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Whether `session_id` is a running Claude Code background session; `None` when the list could
/// not be read, which the caller treats as not listed.
pub(super) async fn listed(session_id: &str) -> Option<bool> {
    let unavailable = |reason: String| {
        tracing::warn!(
            session_id,
            reason,
            "claude background session lookup unavailable; resuming without it"
        );
        None
    };
    let (binary, resolution) = match ClaudeBinary::resolve() {
        Ok(resolved) => resolved,
        Err(error) => return unavailable(error),
    };
    let mut builder = ClaudeCommandBuilder::for_binary(&binary, &resolution);
    builder
        .command_mut()
        .args(["agents", "--json"])
        .stdin(Stdio::null());
    let mut command = tokio::process::Command::from(builder.into_command());
    command.kill_on_drop(true);
    let output = match tokio::time::timeout(LOOKUP_TIMEOUT, command.output()).await {
        Err(_) => return unavailable(format!("timed out after {LOOKUP_TIMEOUT:?}")),
        Ok(Err(error)) => return unavailable(format!("spawn failed: {error}")),
        Ok(Ok(output)) if !output.status.success() => {
            return unavailable(format!("exited with {}", output.status));
        }
        Ok(Ok(output)) => output,
    };
    match serde_json::from_slice::<serde_json::Value>(&output.stdout) {
        Ok(list) => Some(names(&list, session_id)),
        Err(error) => unavailable(format!("unreadable list: {error}")),
    }
}

/// The list's shape is the CLI's, so any string in it equal to the id counts as listing it.
fn names(value: &serde_json::Value, session_id: &str) -> bool {
    match value {
        serde_json::Value::String(text) => text.trim().eq_ignore_ascii_case(session_id),
        serde_json::Value::Array(items) => items.iter().any(|item| names(item, session_id)),
        serde_json::Value::Object(fields) => fields.values().any(|field| names(field, session_id)),
        _ => false,
    }
}
