//! The handoff tail's own origin record binds its Discord turn; every other user record is
//! judged by the turn it names itself.
use super::*;
use crate::services::tui_prompt_dedupe::{self as dedupe, ObservedTuiPrompt};
use serde_json::json;

const TMUX: &str = "AgentDesk-codex-handoff-parser";
const ORIGIN: &str = "[User: probe (ID: 5845)] discord prompt";
const INPUT: &str = "typed into the discord turn";

fn event(kind: &str, turn: &str) -> Value {
    json!({"type": "event_msg", "payload": {"type": kind, "turn_id": turn}})
}

/// A user record as codex-cli 0.160 writes it; `None` drops that metadata field.
fn user(text: &str, turn: Option<&str>, created: Option<chrono::DateTime<chrono::Utc>>) -> Value {
    let mut meta = serde_json::Map::new();
    if let Some(turn) = turn {
        meta.insert("turn_id".into(), json!(turn));
    }
    if let Some(created) = created {
        meta.insert(
            "create_time".into(),
            json!(created.timestamp_millis() as f64 / 1000.0),
        );
    }
    let mut payload = json!({"type": "message", "role": "user",
        "content": [{"type": "input_text", "text": text}]});
    if !meta.is_empty() {
        payload["internal_chat_message_metadata_passthrough"] = Value::Object(meta);
    }
    json!({"type": "response_item", "payload": payload})
}

fn now() -> Option<chrono::DateTime<chrono::Utc>> {
    Some(chrono::Utc::now())
}

/// A Discord handoff reserved as production does before submit, and its tail's parse state.
struct Handoff {
    state: RolloutParseState,
    observed: tokio::sync::broadcast::Receiver<ObservedTuiPrompt>,
    seen: Vec<ObservedTuiPrompt>,
    _guard: dedupe::HandoffGuard,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Handoff {
    fn new() -> Self {
        let lock = dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        dedupe::reset_state_for_tests();
        let guard = dedupe::reserve_discord_handoff("codex", TMUX, ORIGIN);
        dedupe::record_discord_originated_prompt("codex", TMUX, ORIGIN);
        let state = RolloutParseState {
            tmux_session_name: Some(TMUX.to_string()),
            discord_origin_prompt: Some(ORIGIN.to_string()),
            ..RolloutParseState::default()
        };
        Self {
            state,
            observed: dedupe::subscribe_observed_prompts(),
            seen: Vec::new(),
            _guard: guard,
            _lock: lock,
        }
    }

    fn read(&mut self, records: &[Value]) {
        for record in records {
            decode_rollout_record(record, &mut self.state);
        }
    }

    /// `(steer_echo, native turn)` of each event published for `prompt` so far.
    fn published(&mut self, prompt: &str) -> Vec<(bool, Option<String>)> {
        while let Ok(event) = self.observed.try_recv() {
            self.seen.push(event);
        }
        self.seen
            .iter()
            .filter(|event| event.tmux_session_name == TMUX && event.prompt == prompt)
            .map(|event| (event.steer_echo, event.native_turn_id.clone()))
            .collect()
    }
}

fn echo(turn: &str) -> Vec<(bool, Option<String>)> {
    vec![(true, Some(turn.to_string()))]
}

fn owned(turn: &str) -> Vec<(bool, Option<String>)> {
    vec![(false, Some(turn.to_string()))]
}

#[test]
fn an_input_in_the_discord_turn_is_only_echoed() {
    let mut handoff = Handoff::new();
    handoff.read(&[
        event("task_started", "t1"),
        user(ORIGIN, Some("t1"), now()),
        user(INPUT, Some("t1"), now()),
    ]);
    assert_eq!(handoff.published(INPUT), echo("t1"));
    assert!(handoff.published(ORIGIN).is_empty());
}

/// The handoff's turn ended; a record of the next turn read before its `task_started`
/// opens that turn.
#[test]
fn an_input_of_the_next_turn_owns_it() {
    let mut handoff = Handoff::new();
    handoff.read(&[
        event("task_started", "t1"),
        user(ORIGIN, Some("t1"), now()),
        event("task_complete", "t1"),
        user(INPUT, Some("t2"), now()),
        event("task_started", "t2"),
    ]);
    assert_eq!(handoff.published(INPUT), owned("t2"));
}

/// The origin record arrives before any `task_started`: its own turn binds the handoff.
#[test]
fn the_origin_record_binds_its_own_turn_before_task_started() {
    let mut handoff = Handoff::new();
    handoff.read(&[
        user(ORIGIN, Some("t1"), now()),
        event("task_started", "t1"),
        user(INPUT, Some("t1"), now()),
    ]);
    assert_eq!(handoff.published(INPUT), echo("t1"));
}

/// The previous turn ended: the origin binds its own turn, not the last started one, and a
/// later turn's record read before its start is its own input.
#[test]
fn the_origin_record_after_a_finished_turn_binds_its_own_turn() {
    let mut handoff = Handoff::new();
    handoff.read(&[
        event("task_started", "t0"),
        event("task_complete", "t0"),
        user(ORIGIN, Some("t1"), now()),
        event("task_started", "t1"),
        user(INPUT, Some("t1"), now()),
        event("task_complete", "t1"),
        user("next input", Some("t2"), now()),
    ]);
    assert_eq!(handoff.published(INPUT), echo("t1"));
    assert_eq!(handoff.published("next input"), owned("t2"));
}

/// A rollout that names the turn only in `turn_context` still binds; one without record
/// metadata ends the handoff and leaves inputs as before handoffs were reserved.
#[test]
fn turn_context_binds_and_missing_metadata_ends_the_handoff() {
    let mut handoff = Handoff::new();
    handoff.read(&[
        json!({"type": "turn_context", "payload": {"turn_id": "t1"}}),
        user(ORIGIN, Some("t1"), now()),
        user(INPUT, Some("t1"), now()),
    ]);
    assert_eq!(handoff.published(INPUT), echo("t1"));
    drop(handoff);

    let mut handoff = Handoff::new();
    handoff.read(&[user(ORIGIN, None, None), user(INPUT, None, None)]);
    assert_eq!(dedupe::held_handoff_inputs_for_tests("codex", TMUX), None);
    assert_eq!(handoff.published(INPUT), vec![(false, None)]);
}

/// A resumed tail reads an earlier submission of the same text first: it stays suppressed
/// and leaves the origin for this submission's own record.
#[test]
fn an_earlier_record_of_the_origin_text_does_not_take_the_origin() {
    let mut handoff = Handoff::new();
    let earlier = Some(chrono::Utc::now() - chrono::TimeDelta::minutes(5));
    handoff.read(&[
        event("task_started", "t0"),
        user(ORIGIN, Some("t0"), earlier),
        event("task_complete", "t0"),
    ]);
    assert!(handoff.state.discord_origin_prompt.is_some());
    handoff.read(&[
        event("task_started", "t1"),
        user(ORIGIN, Some("t1"), now()),
        user(INPUT, Some("t1"), now()),
        event("task_complete", "t1"),
        user("later input", Some("t2"), now()),
    ]);
    assert!(handoff.state.discord_origin_prompt.is_none());
    assert!(handoff.published(ORIGIN).is_empty());
    assert_eq!(handoff.published(INPUT), echo("t1"));
    assert_eq!(handoff.published("later input"), owned("t2"));
}
