//! Claude Code background sessions (`claude agents --json`), which a resume must not take over.

use std::process::Stdio;
use std::time::Duration;

use crate::services::claude_command::{ClaudeBinary, ClaudeCommandBuilder};

/// A slow resolver or CLI must not hold up the resume, so the whole lookup gives up after this.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Whether `session_id` is a running Claude Code background session under the account home the
/// channel's next launch reads; `None` when that could not be told, which the caller treats as not listed.
pub(super) async fn listed(
    session_id: &str,
    channel_id: Option<u64>,
    tmux_name: &str,
) -> Option<bool> {
    let tmux_name = tmux_name.to_string();
    let lookup = async {
        let prepare = move || agents_command(channel_id, &tmux_name);
        let mut command = tokio::task::spawn_blocking(prepare)
            .await
            .map_err(|error| format!("lookup task failed: {error}"))??;
        command.kill_on_drop(true);
        let output = command.output().await;
        let output = output.map_err(|error| format!("spawn failed: {error}"))?;
        if !output.status.success() {
            return Err(format!("exited with {}", output.status));
        }
        let list = serde_json::from_slice::<serde_json::Value>(&output.stdout);
        lists_background(
            &list.map_err(|error| format!("unreadable list: {error}"))?,
            session_id,
        )
    };
    let reason = match tokio::time::timeout(LOOKUP_TIMEOUT, lookup).await {
        Ok(Ok(listed)) => return Some(listed),
        Ok(Err(reason)) => reason,
        Err(_) => format!("timed out after {LOOKUP_TIMEOUT:?}"),
    };
    tracing::warn!(
        session_id,
        reason,
        "claude background session lookup unavailable; resuming without it"
    );
    None
}

/// `claude agents --json` under the home the read-only launch prediction settles on; an account
/// the next launch could still pick among several is not guessed.
fn agents_command(
    channel_id: Option<u64>,
    tmux_name: &str,
) -> Result<tokio::process::Command, String> {
    let claude = crate::services::provider::ProviderKind::Claude;
    let home =
        crate::services::discord::org_schema::predicted_launch_home(claude, channel_id, tmux_name)?;
    let (binary, resolution) = ClaudeBinary::resolve()?;
    let mut builder = ClaudeCommandBuilder::for_binary(&binary, &resolution);
    let command = builder.command_mut();
    command.args(["agents", "--json"]).stdin(Stdio::null());
    if let Some(home) = home {
        command.env("CLAUDE_CONFIG_DIR", home);
    }
    Ok(tokio::process::Command::from(builder.into_command()))
}

/// The list is an array of sessions whose `kind` tells background from foreground
/// (`interactive`); only a background entry for the id counts, and any other shape is unreadable.
fn lists_background(list: &serde_json::Value, session_id: &str) -> Result<bool, String> {
    let entries = list.as_array().ok_or("the list is not an array")?;
    let mut listed = false;
    for entry in entries {
        let field = |key| entry.get(key).and_then(serde_json::Value::as_str);
        let (Some(kind), Some(id)) = (field("kind"), field("sessionId")) else {
            return Err("a list entry lacks kind or sessionId".to_string());
        };
        listed |= kind == "background" && id.trim().eq_ignore_ascii_case(session_id);
    }
    Ok(listed)
}
