use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::ui::ai_screen::{HistoryItem, HistoryType, SessionData};

use super::super::plan::{ImportPlan, ImportSessionPlan};
use super::SessionMapEntry;
use super::fs_write::write_text_file;

pub(super) fn planned_session_map(plan: &ImportPlan, runtime_root: &Path) -> Vec<SessionMapEntry> {
    let ai_sessions_root = runtime_root.join("ai_sessions");
    plan.sessions
        .iter()
        .map(|session| SessionMapEntry {
            source_agent_id: session.source_agent_id.clone(),
            role_id: session.final_role_id.clone(),
            session_key: session.session_key.clone(),
            session_id: session.session_id.clone(),
            transcript_path: session.transcript_path.clone(),
            ai_session_path: ai_sessions_root
                .join(format!("{}.json", session_output_stem(session)))
                .display()
                .to_string(),
            db_session_key: build_db_session_key(session),
        })
        .collect()
}

pub(super) struct SessionImportOutputs {
    pub(super) written_paths: Vec<String>,
    pub(super) agent_outputs: BTreeMap<String, Vec<String>>,
}

pub(super) fn import_sessions(
    plan: &ImportPlan,
    runtime_root: &Path,
    ai_sessions_root: &Path,
    backups_root: &Path,
    overwrite: bool,
    no_workspace: bool,
) -> Result<SessionImportOutputs, String> {
    let mut written_paths = Vec::new();
    let mut agent_outputs = BTreeMap::<String, Vec<String>>::new();
    let agent_lookup = plan
        .agents
        .iter()
        .map(|agent| (agent.final_role_id.as_str(), agent))
        .collect::<BTreeMap<_, _>>();

    for session in &plan.sessions {
        let output_path = ai_sessions_root.join(format!("{}.json", session_output_stem(session)));
        let Some(agent) = agent_lookup.get(session.final_role_id.as_str()) else {
            continue;
        };
        let history = match session.transcript_path.as_deref() {
            Some(transcript_path) => parse_openclaw_transcript(Path::new(transcript_path))?,
            None => Vec::new(),
        };
        let current_path = if no_workspace {
            session
                .cwd
                .clone()
                .unwrap_or_else(|| agent.workspace_source.clone())
        } else {
            runtime_root
                .join("openclaw")
                .join("workspaces")
                .join(&agent.final_role_id)
                .display()
                .to_string()
        };
        let session_data = SessionData {
            session_id: session.session_id.clone(),
            history,
            current_path,
            created_at: timestamp_to_rfc3339(session.updated_at),
            discord_channel_id: session.channel.as_deref().and_then(parse_u64_str),
            discord_channel_name: session.channel.clone(),
            discord_category_name: None,
            born_generation: 0,
        };
        let rendered = serde_json::to_string_pretty(&session_data)
            .map_err(|e| format!("Failed to serialize '{}': {e}", output_path.display()))?;
        write_text_file(
            &output_path,
            &rendered,
            runtime_root,
            backups_root,
            overwrite,
        )?;
        let rendered_path = output_path.display().to_string();
        written_paths.push(rendered_path.clone());
        agent_outputs
            .entry(session.final_role_id.clone())
            .or_default()
            .push(rendered_path);
    }

    Ok(SessionImportOutputs {
        written_paths,
        agent_outputs,
    })
}

fn parse_openclaw_transcript(path: &Path) -> Result<Vec<HistoryItem>, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    let mut history = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) == Some("session") {
            continue;
        }

        let message = value.get("message").unwrap_or(&value);
        let role = message
            .get("role")
            .and_then(serde_json::Value::as_str)
            .or_else(|| value.get("role").and_then(serde_json::Value::as_str));
        let Some(role) = role else {
            continue;
        };
        let items =
            extract_transcript_history_items(message.get("content").unwrap_or(message), role);
        if items.is_empty() {
            if let Some(content) =
                extract_transcript_text(&value).filter(|text| !text.trim().is_empty())
            {
                history.push(HistoryItem {
                    item_type: map_transcript_role(role),
                    content,
                });
            }
            continue;
        }
        history.extend(items);
    }

    Ok(history)
}

fn extract_transcript_history_items(
    value: &serde_json::Value,
    fallback_role: &str,
) -> Vec<HistoryItem> {
    let Some(items) = value.as_array() else {
        return extract_transcript_text(value)
            .filter(|text| !text.trim().is_empty())
            .map(|content| {
                vec![HistoryItem {
                    item_type: map_transcript_role(fallback_role),
                    content,
                }]
            })
            .unwrap_or_default();
    };

    let mut history = Vec::new();
    for item in items {
        let item_type = item
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(|value| value.trim().to_ascii_lowercase());
        match item_type.as_deref() {
            Some("tool_use") | Some("toolcall") | Some("tool_call") => {
                let tool_name = item
                    .get("name")
                    .or_else(|| item.get("tool"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("tool");
                let payload = item
                    .get("input")
                    .or_else(|| item.get("arguments"))
                    .map(render_transcript_block)
                    .unwrap_or_default();
                let content = if payload.is_empty() {
                    tool_name.to_string()
                } else {
                    format!("{tool_name}\n{payload}")
                };
                history.push(HistoryItem {
                    item_type: HistoryType::ToolUse,
                    content,
                });
            }
            Some("tool_result") | Some("tool_result_error") => {
                if let Some(content) = item
                    .get("result")
                    .or_else(|| item.get("output"))
                    .or_else(|| item.get("text"))
                    .map(render_transcript_block)
                    .filter(|text| !text.trim().is_empty())
                {
                    history.push(HistoryItem {
                        item_type: HistoryType::ToolResult,
                        content,
                    });
                }
            }
            _ => {
                if let Some(content) =
                    extract_transcript_text(item).filter(|text| !text.trim().is_empty())
                {
                    history.push(HistoryItem {
                        item_type: map_transcript_role(fallback_role),
                        content,
                    });
                }
            }
        }
    }

    history
}

fn extract_transcript_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    if let Some(object) = value.as_object() {
        if let Some(text) = object.get("text").and_then(serde_json::Value::as_str) {
            return Some(text.to_string());
        }
        if let Some(content) = object.get("content") {
            return extract_transcript_text(content);
        }
    }
    let Some(items) = value.as_array() else {
        return None;
    };
    let parts = items
        .iter()
        .filter_map(|item| {
            item.get("text")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| item.as_str().map(ToOwned::to_owned))
        })
        .collect::<Vec<_>>();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

fn render_transcript_block(value: &serde_json::Value) -> String {
    if let Some(text) = extract_transcript_text(value) {
        return text;
    }
    if value.is_null() {
        return String::new();
    }
    serde_json::to_string(value).unwrap_or_default()
}

fn map_transcript_role(role: &str) -> HistoryType {
    match role.trim().to_ascii_lowercase().as_str() {
        "user" => HistoryType::User,
        "assistant" => HistoryType::Assistant,
        "system" => HistoryType::System,
        "tool" => HistoryType::ToolResult,
        _ => HistoryType::Assistant,
    }
}

fn build_db_session_key(session: &ImportSessionPlan) -> String {
    format!("openclaw:{}:{}", session.final_role_id, session.session_key)
}

fn session_output_stem(session: &ImportSessionPlan) -> String {
    let base = format!("openclaw-{}-{}", session.final_role_id, session.session_id);
    let sanitized = sanitize_file_component(&base);
    let mut hasher = Sha256::new();
    hasher.update(session.session_key.as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("{}-{}", sanitized, &digest[..8])
}

fn sanitize_file_component(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').to_string()
}

fn timestamp_to_rfc3339(updated_at_ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(updated_at_ms)
        .unwrap_or_else(Utc::now)
        .to_rfc3339()
}

pub(super) fn parse_u64_str(raw: &str) -> Option<u64> {
    raw.trim().parse::<u64>().ok()
}
