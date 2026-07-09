//! Stable, machine-readable JSON projections for the CLI commands that
//! historically emitted human-only text (issue #4224: `status`, `cards`,
//! `queue`, `advance`, `terminations`).
//!
//! These are **pure** `Value -> Value` transformers. The caller
//! (`super::client`) owns HTTP fetching and stdout emission, which keeps this
//! module free of side effects and trivially unit-testable. Shapes use
//! `snake_case` keys, carry no ANSI escapes, and never embed human prose inside
//! values — a script can consume them without re-parsing sentences.

use serde_json::{Value, json};

/// Read a string field, returning `Value::Null` when absent so the emitted
/// shape stays stable (the key is always present).
fn str_field(source: &Value, key: &str) -> Value {
    match source.get(key).and_then(Value::as_str) {
        Some(value) => Value::String(value.to_string()),
        None => Value::Null,
    }
}

/// Read an integer field, returning `Value::Null` when absent/non-integer.
fn i64_field(source: &Value, key: &str) -> Value {
    match source.get(key).and_then(Value::as_i64) {
        Some(value) => json!(value),
        None => Value::Null,
    }
}

/// `agentdesk status --json`
///
/// Projects the same facts the text table shows — server status/version,
/// Discord provider health, session tallies and the auto-queue summary — but
/// as structured fields. Tolerant of partial responses: missing arrays degrade
/// to empty counts rather than an error, so the object shape is always valid.
pub(crate) fn status(base_url: &str, health: &Value, sessions: &Value, queue: &Value) -> Value {
    let version = health
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let server_status = health
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            let ok = health.get("ok").and_then(Value::as_bool).unwrap_or(false);
            let db = health.get("db").and_then(Value::as_bool).unwrap_or(false);
            if ok && db { "healthy" } else { "degraded" }.to_string()
        });

    let sessions_list = sessions
        .get("sessions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total_sessions = sessions_list.len();
    let working_sessions = sessions_list
        .iter()
        .filter(|session| {
            matches!(
                session.get("status").and_then(Value::as_str),
                Some("turn_active" | "awaiting_bg" | "working")
            )
        })
        .count();
    let active_dispatch_sessions = sessions_list
        .iter()
        .filter(|session| {
            !session
                .get("active_dispatch_id")
                .unwrap_or(&Value::Null)
                .is_null()
        })
        .count();

    let queue_entries = queue
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let run = queue.get("run").and_then(Value::as_object);
    let queue_status = run
        .and_then(|run| run.get("status").and_then(Value::as_str))
        .map(str::to_string);
    let queue_agent_id = run
        .and_then(|run| run.get("agent_id").and_then(Value::as_str))
        .map(str::to_string);

    json!({
        "base_url": base_url,
        "server": {
            "status": server_status,
            "version": version,
        },
        "discord": discord_summary(health),
        "sessions": {
            "total": total_sessions,
            "working": working_sessions,
            "with_active_dispatch": active_dispatch_sessions,
        },
        "queue": {
            "status": queue_status,
            "agent_id": queue_agent_id,
            "total": queue_entries.len(),
            "pending": count_status(&queue_entries, "pending"),
            "dispatched": count_status(&queue_entries, "dispatched"),
            "done": count_status(&queue_entries, "done"),
            "failed": count_status(&queue_entries, "failed"),
            "skipped": count_status(&queue_entries, "skipped"),
        },
    })
}

/// Structured Discord provider health for the `status` JSON payload. Mirrors
/// the data behind the text one-liner but keeps the connected/offline provider
/// names as arrays instead of a prose string.
fn discord_summary(health: &Value) -> Value {
    let Some(providers) = health.get("providers").and_then(Value::as_array) else {
        return json!({
            "available": false,
            "total": 0,
            "connected": [],
            "offline": [],
        });
    };
    let connected: Vec<String> = providers
        .iter()
        .filter(|p| p.get("connected").and_then(Value::as_bool) == Some(true))
        .filter_map(|p| p.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    let offline: Vec<String> = providers
        .iter()
        .filter(|p| p.get("connected").and_then(Value::as_bool) != Some(true))
        .filter_map(|p| p.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    json!({
        "available": true,
        "total": providers.len(),
        "connected": connected,
        "offline": offline,
    })
}

fn count_status(entries: &[Value], status: &str) -> usize {
    entries
        .iter()
        .filter(|entry| entry.get("status").and_then(Value::as_str) == Some(status))
        .count()
}

/// `agentdesk cards --json`
///
/// Projects one object per card carrying the columns the text table renders,
/// plus the raw `id`, so scripts can key on a card without scraping the table.
pub(crate) fn cards(cards: &[Value]) -> Value {
    let rows: Vec<Value> = cards.iter().map(card_row).collect();
    json!({ "cards": rows })
}

fn card_row(card: &Value) -> Value {
    let agent = card
        .get("assigned_agent_id")
        .and_then(Value::as_str)
        .or_else(|| card.get("assignee_agent_id").and_then(Value::as_str))
        .map(|value| Value::String(value.to_string()))
        .unwrap_or(Value::Null);
    json!({
        "id": str_field(card, "id"),
        "github_issue_number": i64_field(card, "github_issue_number"),
        "status": str_field(card, "status"),
        "review_status": str_field(card, "review_status"),
        "priority": str_field(card, "priority"),
        "agent_id": agent,
        "title": str_field(card, "title"),
    })
}

/// `agentdesk queue --json`
///
/// Projects the auto-queue run header plus one object per entry, with each
/// entry's thread links normalised to `{label, url, thread_id}`.
pub(crate) fn queue(data: &Value) -> Value {
    let run = &data["run"];
    let entries: Vec<Value> = data["entries"]
        .as_array()
        .map(|entries| entries.iter().map(queue_entry).collect())
        .unwrap_or_default();
    json!({
        "run": {
            "status": str_field(run, "status"),
            "unified_thread": run.get("unified_thread").and_then(Value::as_bool).unwrap_or(false),
            "max_concurrent_threads": run.get("max_concurrent_threads").and_then(Value::as_i64).unwrap_or(1),
        },
        "entries": entries,
    })
}

fn queue_entry(entry: &Value) -> Value {
    let links: Vec<Value> = entry
        .get("thread_links")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|link| {
            json!({
                "label": str_field(link, "label"),
                "url": str_field(link, "url"),
                "thread_id": str_field(link, "thread_id"),
            })
        })
        .collect();
    json!({
        "github_issue_number": i64_field(entry, "github_issue_number"),
        "status": str_field(entry, "status"),
        "card_title": str_field(entry, "card_title"),
        "thread_links": links,
    })
}

/// `agentdesk advance <n> --json`
///
/// Reports the terminal outcome of the advance action as a single object.
/// `outcome` is a stable token (`advanced_to_review` or `already_in_review`);
/// `completed_dispatch_id` is present only when this invocation completed a
/// pending dispatch.
pub(crate) fn advance(
    issue_number: &str,
    card_id: &str,
    outcome: &str,
    review_dispatch_id: &str,
    completed_dispatch_id: Option<&str>,
) -> Value {
    // `issue_number` is validated as an integer upstream (find_card_for_issue),
    // so emit it as a JSON number for consistency with the `github_issue_number`
    // fields the other shapes carry (#4372 r3). Fall back to a string only if a
    // non-numeric value ever reaches here.
    let issue_value = issue_number
        .parse::<i64>()
        .map(Value::from)
        .unwrap_or_else(|_| Value::String(issue_number.to_string()));
    json!({
        "issue_number": issue_value,
        "card_id": card_id,
        "outcome": outcome,
        "review_dispatch_id": review_dispatch_id,
        "completed_dispatch_id": completed_dispatch_id,
    })
}

/// `agentdesk terminations --json`
///
/// Projects one object per session-termination event carrying the columns the
/// text table renders. `tmux_alive` stays a genuine boolean (or null).
pub(crate) fn terminations(events: &[Value]) -> Value {
    let rows: Vec<Value> = events
        .iter()
        .map(|event| {
            json!({
                "created_at": str_field(event, "created_at"),
                "killer_component": str_field(event, "killer_component"),
                "reason_code": str_field(event, "reason_code"),
                "session_key": str_field(event, "session_key"),
                "tmux_alive": event.get("tmux_alive").and_then(Value::as_bool),
                "reason_text": str_field(event, "reason_text"),
            })
        })
        .collect();
    json!({ "events": rows })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_projects_counts_and_discord() {
        let health = json!({
            "status": "healthy",
            "version": "9.9.9",
            "providers": [
                {"name": "announce", "connected": true},
                {"name": "cc", "connected": false},
            ],
        });
        let sessions = json!({
            "sessions": [
                {"status": "turn_active", "active_dispatch_id": "d1"},
                {"status": "idle", "active_dispatch_id": null},
            ]
        });
        let queue = json!({
            "run": {"status": "running", "agent_id": "agent-x"},
            "entries": [
                {"status": "pending"},
                {"status": "dispatched"},
                {"status": "done"},
            ],
        });

        let out = status("http://127.0.0.1:8080", &health, &sessions, &queue);
        assert_eq!(out["base_url"], "http://127.0.0.1:8080");
        assert_eq!(out["server"]["status"], "healthy");
        assert_eq!(out["server"]["version"], "9.9.9");
        assert_eq!(out["discord"]["available"], true);
        assert_eq!(out["discord"]["total"], 2);
        assert_eq!(out["discord"]["connected"], json!(["announce"]));
        assert_eq!(out["discord"]["offline"], json!(["cc"]));
        assert_eq!(out["sessions"]["total"], 2);
        assert_eq!(out["sessions"]["working"], 1);
        assert_eq!(out["sessions"]["with_active_dispatch"], 1);
        assert_eq!(out["queue"]["status"], "running");
        assert_eq!(out["queue"]["agent_id"], "agent-x");
        assert_eq!(out["queue"]["total"], 3);
        assert_eq!(out["queue"]["pending"], 1);
        assert_eq!(out["queue"]["dispatched"], 1);
        assert_eq!(out["queue"]["done"], 1);
    }

    #[test]
    fn status_is_tolerant_of_missing_sections() {
        let out = status("u", &json!({}), &json!({}), &json!({}));
        // Degraded fallback + zeroed tallies, still a valid object.
        assert_eq!(out["server"]["status"], "degraded");
        assert_eq!(out["server"]["version"], "unknown");
        assert_eq!(out["discord"]["available"], false);
        assert_eq!(out["sessions"]["total"], 0);
        assert_eq!(out["queue"]["total"], 0);
        assert_eq!(out["queue"]["status"], Value::Null);
    }

    #[test]
    fn cards_projects_columns_with_agent_fallback() {
        let input = vec![
            json!({
                "id": "card-1",
                "github_issue_number": 4224,
                "status": "in_progress",
                "review_status": "pending",
                "priority": "high",
                "assignee_agent_id": "agent-fallback",
                "title": "Add --json",
            }),
            json!({"id": "card-2", "title": "no issue", "status": "backlog"}),
        ];
        let out = cards(&input);
        let rows = out["cards"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["github_issue_number"], 4224);
        assert_eq!(rows[0]["agent_id"], "agent-fallback");
        assert_eq!(rows[0]["review_status"], "pending");
        // Missing fields degrade to null, not a panic or dropped key.
        assert_eq!(rows[1]["github_issue_number"], Value::Null);
        assert_eq!(rows[1]["agent_id"], Value::Null);
        assert_eq!(rows[1]["priority"], Value::Null);
    }

    #[test]
    fn cards_empty_is_empty_array() {
        assert_eq!(cards(&[]), json!({ "cards": [] }));
    }

    #[test]
    fn queue_projects_run_and_entries() {
        let data = json!({
            "run": {"status": "running", "unified_thread": true, "max_concurrent_threads": 3},
            "entries": [
                {
                    "github_issue_number": 42,
                    "status": "dispatched",
                    "card_title": "Title",
                    "thread_links": [
                        {"label": "work", "url": "https://x/1"},
                        {"label": "review", "thread_id": "t2"},
                    ],
                }
            ],
        });
        let out = queue(&data);
        assert_eq!(out["run"]["status"], "running");
        assert_eq!(out["run"]["unified_thread"], true);
        assert_eq!(out["run"]["max_concurrent_threads"], 3);
        let entries = out["entries"].as_array().unwrap();
        assert_eq!(entries[0]["github_issue_number"], 42);
        assert_eq!(entries[0]["thread_links"][0]["label"], "work");
        assert_eq!(entries[0]["thread_links"][0]["url"], "https://x/1");
        assert_eq!(entries[0]["thread_links"][0]["thread_id"], Value::Null);
        assert_eq!(entries[0]["thread_links"][1]["thread_id"], "t2");
    }

    #[test]
    fn queue_defaults_when_run_missing() {
        let out = queue(&json!({"entries": []}));
        assert_eq!(out["run"]["status"], Value::Null);
        assert_eq!(out["run"]["unified_thread"], false);
        assert_eq!(out["run"]["max_concurrent_threads"], 1);
        assert_eq!(out["entries"], json!([]));
    }

    #[test]
    fn advance_outcomes() {
        let advanced = advance(
            "42",
            "card-1",
            "advanced_to_review",
            "rev-1",
            Some("disp-1"),
        );
        // issue_number is emitted numerically (#4372 r3), not as a string.
        assert_eq!(advanced["issue_number"], json!(42));
        assert!(advanced["issue_number"].is_number());
        assert_eq!(advanced["card_id"], "card-1");
        assert_eq!(advanced["outcome"], "advanced_to_review");
        assert_eq!(advanced["review_dispatch_id"], "rev-1");
        assert_eq!(advanced["completed_dispatch_id"], "disp-1");

        let already = advance("42", "card-1", "already_in_review", "rev-9", None);
        assert_eq!(already["outcome"], "already_in_review");
        assert_eq!(already["completed_dispatch_id"], Value::Null);
    }

    #[test]
    fn advance_issue_number_falls_back_to_string_when_non_numeric() {
        let out = advance("not-a-number", "c", "advanced_to_review", "r", None);
        assert_eq!(out["issue_number"], "not-a-number");
    }

    #[test]
    fn terminations_projects_events_with_bool_alive() {
        let events = vec![json!({
            "created_at": "2026-07-09T00:00:00Z",
            "killer_component": "watchdog",
            "reason_code": "stall_timeout",
            "session_key": "AgentDesk-x",
            "tmux_alive": false,
            "reason_text": "no output",
        })];
        let out = terminations(&events);
        let rows = out["events"].as_array().unwrap();
        assert_eq!(rows[0]["killer_component"], "watchdog");
        assert_eq!(rows[0]["reason_code"], "stall_timeout");
        assert_eq!(rows[0]["tmux_alive"], false);
        assert_eq!(rows[0]["reason_text"], "no output");
    }

    #[test]
    fn terminations_empty_is_empty_array() {
        assert_eq!(terminations(&[]), json!({ "events": [] }));
    }
}
