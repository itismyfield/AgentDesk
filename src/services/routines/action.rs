use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum RoutineAction {
    Complete {
        result_json: Option<Value>,
        checkpoint: Option<Value>,
        last_result: Option<String>,
        next_due_at: Option<DateTime<Utc>>,
    },
    Skip {
        reason: Option<String>,
        result_json: Option<Value>,
        checkpoint: Option<Value>,
        last_result: Option<String>,
        next_due_at: Option<DateTime<Utc>>,
    },
    Pause {
        reason: Option<String>,
        result_json: Option<Value>,
        checkpoint: Option<Value>,
        last_result: Option<String>,
    },
    Agent {
        prompt: String,
        dm_user_id: Option<String>,
        checkpoint: Option<Value>,
        last_result: Option<String>,
        next_due_at: Option<DateTime<Utc>>,
    },
}

impl RoutineAction {
    pub fn validate(value: Value) -> Result<Self> {
        validate_routine_action(value)
    }

    pub fn action_name(&self) -> &'static str {
        match self {
            Self::Complete { .. } => "complete",
            Self::Skip { .. } => "skip",
            Self::Pause { .. } => "pause",
            Self::Agent { .. } => "agent",
        }
    }
}

pub fn validate_routine_action(value: Value) -> Result<RoutineAction> {
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow!("RoutineAction must be an object"))?;

    if obj.contains_key("kind") && !obj.contains_key("action") {
        bail!("RoutineAction.kind is metadata only; use action to select runtime behavior");
    }

    let action = required_string(obj, "action")?;
    match action.as_str() {
        "complete" => {
            reject_unknown_keys(
                obj,
                &[
                    "action",
                    "kind",
                    "result",
                    "checkpoint",
                    "lastResult",
                    "last_result",
                    "nextDueAt",
                    "next_due_at",
                ],
            )?;
            Ok(RoutineAction::Complete {
                result_json: optional_value(obj, "result"),
                checkpoint: optional_value(obj, "checkpoint"),
                last_result: optional_string_alias(obj, "lastResult", "last_result")?,
                next_due_at: optional_datetime_alias(obj, "nextDueAt", "next_due_at")?,
            })
        }
        "skip" => {
            reject_unknown_keys(
                obj,
                &[
                    "action",
                    "kind",
                    "reason",
                    "result",
                    "checkpoint",
                    "lastResult",
                    "last_result",
                    "nextDueAt",
                    "next_due_at",
                ],
            )?;
            Ok(RoutineAction::Skip {
                reason: optional_string(obj, "reason")?,
                result_json: optional_value(obj, "result"),
                checkpoint: optional_value(obj, "checkpoint"),
                last_result: optional_string_alias(obj, "lastResult", "last_result")?,
                next_due_at: optional_datetime_alias(obj, "nextDueAt", "next_due_at")?,
            })
        }
        "pause" => {
            reject_unknown_keys(
                obj,
                &[
                    "action",
                    "kind",
                    "reason",
                    "result",
                    "checkpoint",
                    "lastResult",
                    "last_result",
                ],
            )?;
            Ok(RoutineAction::Pause {
                reason: optional_string(obj, "reason")?,
                result_json: optional_value(obj, "result"),
                checkpoint: optional_value(obj, "checkpoint"),
                last_result: optional_string_alias(obj, "lastResult", "last_result")?,
            })
        }
        "agent" => {
            reject_unknown_keys(
                obj,
                &[
                    "action",
                    "kind",
                    "prompt",
                    "dmUserId",
                    "dm_user_id",
                    "checkpoint",
                    "lastResult",
                    "last_result",
                    "nextDueAt",
                    "next_due_at",
                ],
            )?;
            let prompt = required_string(obj, "prompt")?;
            if prompt.trim().is_empty() {
                bail!("RoutineAction.agent.prompt must not be empty");
            }
            Ok(RoutineAction::Agent {
                prompt,
                dm_user_id: optional_discord_snowflake_alias(obj, "dmUserId", "dm_user_id")?,
                checkpoint: optional_value(obj, "checkpoint"),
                last_result: optional_string_alias(obj, "lastResult", "last_result")?,
                next_due_at: optional_datetime_alias(obj, "nextDueAt", "next_due_at")?,
            })
        }
        other => bail!(
            "unsupported RoutineAction.action '{other}'; expected complete, skip, pause, or agent"
        ),
    }
}

fn reject_unknown_keys(obj: &Map<String, Value>, allowed: &[&str]) -> Result<()> {
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            bail!("unsupported RoutineAction field '{key}'");
        }
    }
    Ok(())
}

fn required_string(obj: &Map<String, Value>, key: &str) -> Result<String> {
    match obj.get(key) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => bail!("RoutineAction.{key} must be a string"),
        None => bail!("RoutineAction.{key} is required"),
    }
}

fn optional_value(obj: &Map<String, Value>, key: &str) -> Option<Value> {
    match obj.get(key) {
        Some(Value::Null) | None => None,
        Some(value) => Some(value.clone()),
    }
}

fn optional_string_alias(
    obj: &Map<String, Value>,
    primary: &str,
    alias: &str,
) -> Result<Option<String>> {
    match optional_string(obj, primary)? {
        Some(value) => Ok(Some(value)),
        None => optional_string(obj, alias),
    }
}

fn optional_discord_snowflake_alias(
    obj: &Map<String, Value>,
    primary: &str,
    alias: &str,
) -> Result<Option<String>> {
    let value = optional_string_alias(obj, primary, alias)?;
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        bail!("RoutineAction.{primary} must be a Discord snowflake string");
    }
    Ok(Some(trimmed.to_string()))
}

fn optional_string(obj: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match obj.get(key) {
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => bail!("RoutineAction.{key} must be a string"),
    }
}

fn optional_datetime_alias(
    obj: &Map<String, Value>,
    primary: &str,
    alias: &str,
) -> Result<Option<DateTime<Utc>>> {
    match optional_datetime(obj, primary)? {
        Some(value) => Ok(Some(value)),
        None => optional_datetime(obj, alias),
    }
}

fn optional_datetime(obj: &Map<String, Value>, key: &str) -> Result<Option<DateTime<Utc>>> {
    let Some(value) = optional_string(obj, key)? else {
        return Ok(None);
    };
    DateTime::parse_from_rfc3339(&value)
        .map(|dt| Some(dt.with_timezone(&Utc)))
        .map_err(|e| anyhow!("RoutineAction.{key} must be RFC3339 datetime: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_complete_action_with_checkpoint() {
        let action = RoutineAction::validate(json!({
            "action": "complete",
            "result": {"ok": true},
            "checkpoint": {"cursor": "abc"},
            "lastResult": "ok",
            "nextDueAt": "2026-04-29T01:00:00Z"
        }))
        .unwrap();

        match action {
            RoutineAction::Complete {
                result_json,
                checkpoint,
                last_result,
                next_due_at,
            } => {
                assert_eq!(result_json, Some(json!({"ok": true})));
                assert_eq!(checkpoint, Some(json!({"cursor": "abc"})));
                assert_eq!(last_result.as_deref(), Some("ok"));
                assert!(next_due_at.is_some());
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn rejects_kind_as_runtime_selector() {
        let err = RoutineAction::validate(json!({"kind": "complete"})).unwrap_err();
        assert!(err.to_string().contains("use action"));
    }

    #[test]
    fn rejects_unknown_action() {
        let err = RoutineAction::validate(json!({"action": "retry"})).unwrap_err();
        assert!(err.to_string().contains("unsupported RoutineAction.action"));
    }

    #[test]
    fn validates_agent_prompt_contract() {
        let action = RoutineAction::validate(json!({
            "action": "agent",
            "prompt": "summarize current queue",
            "checkpoint": {"cursor": 3},
            "nextDueAt": "2026-04-29T02:00:00Z"
        }))
        .unwrap();
        assert_eq!(action.action_name(), "agent");
        match action {
            RoutineAction::Agent {
                prompt,
                dm_user_id,
                checkpoint,
                last_result,
                next_due_at,
            } => {
                assert_eq!(prompt, "summarize current queue");
                assert_eq!(dm_user_id, None);
                assert_eq!(checkpoint, Some(json!({"cursor": 3})));
                assert_eq!(last_result, None);
                assert!(next_due_at.is_some());
            }
            other => panic!("unexpected action: {other:?}"),
        }

        let err = RoutineAction::validate(json!({"action": "agent", "prompt": "   "})).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn validates_agent_action_with_last_result() {
        // Regression for #3563: dispatch-label routines (e.g.
        // dependency-update-watcher) return `{action:"agent", ...,
        // lastResult:"...dispatched"}`. Both the camelCase and snake_case
        // spellings must validate into RoutineAction::Agent rather than being
        // rejected by reject_unknown_keys (which previously persisted the
        // rejection error as last_result via fail_run, surfacing in GET
        // /api/routines).
        for key in ["lastResult", "last_result"] {
            let action = RoutineAction::validate(json!({
                "action": "agent",
                "prompt": "run dependency update check",
                key: "dependency update check dispatched"
            }))
            .unwrap();
            match action {
                RoutineAction::Agent { last_result, .. } => {
                    assert_eq!(
                        last_result.as_deref(),
                        Some("dependency update check dispatched"),
                        "{key} alias must populate last_result"
                    );
                }
                other => panic!("unexpected action: {other:?}"),
            }
        }
    }

    #[test]
    fn validates_agent_dm_user_id_aliases() {
        let action = RoutineAction::validate(json!({
            "action": "agent",
            "prompt": "ask in DM",
            "dmUserId": "343742347365974026"
        }))
        .unwrap();
        match action {
            RoutineAction::Agent { dm_user_id, .. } => {
                assert_eq!(dm_user_id.as_deref(), Some("343742347365974026"));
            }
            other => panic!("unexpected action: {other:?}"),
        }

        let err = RoutineAction::validate(json!({
            "action": "agent",
            "prompt": "ask in DM",
            "dm_user_id": "not-a-snowflake"
        }))
        .unwrap_err();
        assert!(err.to_string().contains("Discord snowflake"));
    }
}
