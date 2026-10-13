use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, mpsc};
use std::time::Duration;

const TMUX: &str = "AgentDesk-codex-handoff-unit";
const ORIGIN: &str = "[User: probe (ID: 5845)] discord prompt";
const INPUT: &str = "typed while the discord prompt was submitted";

fn locked() -> std::sync::MutexGuard<'static, ()> {
    let guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    reset_state_for_tests();
    guard
}

fn hook(prompt: &str, turn: &str) -> PromptObservation {
    observe_codex_prompt_in_turn_at(TMUX, prompt, None, Some(turn), Utc::now())
}

fn record(turn: Option<&str>, created: Option<DateTime<Utc>>) -> RecordEvidence {
    RecordEvidence {
        turn_id: turn.map(str::to_string),
        create_time: created,
    }
}

/// Events published on this test's session, kept so each prompt can be asked for in turn.
struct Published {
    rx: tokio::sync::broadcast::Receiver<ObservedTuiPrompt>,
    seen: Vec<ObservedTuiPrompt>,
}

impl Published {
    fn new() -> Self {
        let rx = subscribe_observed_prompts();
        Self { rx, seen: Vec::new() }
    }

    /// `(steer_echo, source_event_id)` of each event published for `prompt` so far.
    fn of(&mut self, prompt: &str) -> Vec<(bool, Option<String>)> {
        while let Ok(event) = self.rx.try_recv() {
            self.seen.push(event);
        }
        self.seen
            .iter()
            .filter(|event| event.tmux_session_name == TMUX && event.prompt == prompt)
            .map(|event| (event.steer_echo, event.source_event_id.clone()))
            .collect()
    }
}

fn slot(turn: &str) -> Option<NativeTurnOwner> {
    let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state
        .native_turn_by_tmux
        .get(&PromptKey::new("codex", TMUX))
        .filter(|slot| slot.value.turn == turn)
        .map(|slot| slot.value.owner)
}

/// A hook that names no entry id is held, then observed exactly once with no id when its
/// handoff ends unbound; a second copy with an id merges into the held one.
#[test]
fn a_held_input_is_observed_once_when_its_handoff_ends_unbound() {
    let _lock = locked();
    let mut rx = Published::new();
    let guard = reserve_discord_handoff("codex", TMUX, ORIGIN);
    assert_eq!(hook(INPUT, "t1"), PromptObservation::ParkedForHandoff);
    assert_eq!(held_handoff_inputs_for_tests("codex", TMUX), Some(1));
    assert!(rx.of(INPUT).is_empty(), "held, not published");
    drop(guard);
    assert_eq!(held_handoff_inputs_for_tests("codex", TMUX), None);
    assert_eq!(rx.of(INPUT), vec![(false, None)]);

    let guard = reserve_discord_handoff("codex", TMUX, ORIGIN);
    let now = Utc::now();
    let seen = |id| observe_codex_prompt_in_turn_at(TMUX, "second input", Some(id), Some("t2"), now);
    assert_eq!(seen("e1"), PromptObservation::ParkedForHandoff);
    assert_eq!(seen("e2"), PromptObservation::ParkedForHandoff);
    assert_eq!(held_handoff_inputs_for_tests("codex", TMUX), Some(1));
    drop(guard);
    assert_eq!(
        rx.of("second input"),
        vec![(false, Some("e1".to_string()))]
    );
    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at("codex", TMUX, "second input", Some("e2"), now),
        PromptObservation::SuppressedReplayedEntry,
        "the merged copy's id is recorded too"
    );
}

/// A record that names its turn but no creation time proves nothing: the reservation ends
/// as if there were none, and what it held is published as its own input.
#[test]
fn a_partial_record_ends_the_handoff_without_an_answer_slot() {
    let _lock = locked();
    let line = serde_json::json!({"type": "response_item", "payload": {
        "type": "message", "role": "user", "content": [{"type": "input_text", "text": ORIGIN}],
        "internal_chat_message_metadata_passthrough": {"turn_id": "t1"}}});
    let (_, _, partial) = extract_codex_rollout_user_prompt_with_evidence(&line).unwrap();
    assert_eq!(partial, record(Some("t1"), None));
    let mut rx = Published::new();
    let _guard = reserve_discord_handoff("codex", TMUX, ORIGIN);
    assert_eq!(hook(INPUT, "t1"), PromptObservation::ParkedForHandoff);
    assert_eq!(
        bind_discord_handoff_record(TMUX, ORIGIN, &partial),
        BindOutcome::NoEvidence
    );
    assert_eq!(held_handoff_inputs_for_tests("codex", TMUX), None);
    assert_eq!(rx.of(INPUT), vec![(false, None)]);
    assert_eq!(slot("t1"), Some(NativeTurnOwner::Relay));
}

/// A complete record binds; the held input of the same turn is only echoed, and one of a
/// later turn is published as its own.
#[test]
fn a_complete_record_binds_and_settles_held_inputs_by_turn() {
    let _lock = locked();
    let mut rx = Published::new();
    let _guard = reserve_discord_handoff("codex", TMUX, ORIGIN);
    assert_eq!(hook(INPUT, "t1"), PromptObservation::ParkedForHandoff);
    assert_eq!(hook("next turn input", "t2"), PromptObservation::ParkedForHandoff);
    let line = serde_json::json!({"type": "response_item", "payload": {
        "type": "message", "role": "user", "content": [{"type": "input_text", "text": ORIGIN}],
        "internal_chat_message_metadata_passthrough": {"turn_id": "t1",
            "create_time": Utc::now().timestamp_millis() as f64 / 1000.0}}});
    let (_, _, evidence) = extract_codex_rollout_user_prompt_with_evidence(&line).unwrap();
    assert_eq!(
        bind_discord_handoff_record(TMUX, ORIGIN, &evidence),
        BindOutcome::Bound
    );
    let events = rx.of(INPUT);
    assert_eq!(events, vec![(true, None)], "joins the Discord turn");
    assert_eq!(rx.of("next turn input"), vec![(false, None)]);
    assert_eq!(slot("t2"), Some(NativeTurnOwner::Relay));
    let stale = record(Some("t0"), Some(Utc::now() - chrono::TimeDelta::minutes(5)));
    assert_eq!(
        bind_discord_handoff_record(TMUX, ORIGIN, &stale),
        BindOutcome::Conflict
    );
}

/// A reservation that outlives its TTL still publishes what it held, exactly once.
#[test]
fn an_expired_handoff_publishes_its_held_input_once() {
    let _lock = locked();
    let mut rx = Published::new();
    std::mem::forget(reserve_discord_handoff("codex", TMUX, ORIGIN));
    assert_eq!(hook(INPUT, "t1"), PromptObservation::ParkedForHandoff);
    Instant::advance(PROMPT_ANCHOR_SUBMIT_TTL - Duration::from_secs(1));
    drain_expired_handoffs();
    assert!(rx.of(INPUT).is_empty(), "still held before the TTL");
    Instant::advance(Duration::from_secs(2));
    drain_expired_handoffs();
    drain_expired_handoffs();
    assert_eq!(held_handoff_inputs_for_tests("codex", TMUX), None);
    assert_eq!(rx.of(INPUT), vec![(false, None)]);
}

/// A handoff reserved and bound while an input's owning decision is in progress waits for
/// that decision: the input owns its turn and the bind leaves its Relay slot in place.
#[test]
fn a_bind_waits_for_an_owning_decision_in_progress() {
    let _lock = locked();
    let mut rx = Published::new();
    let (during_tx, during_rx) = mpsc::channel::<bool>();
    let (bound_tx, bound_rx) = mpsc::channel::<BindOutcome>();
    let pending = std::sync::Mutex::new(Some((during_tx, bound_tx)));
    let _probe = probes::on_owning_commit(move |prompt| {
        let Some((during_tx, bound_tx)) = pending.lock().unwrap().take().filter(|_| prompt == INPUT)
        else {
            return;
        };
        let done = Arc::new(AtomicBool::new(false));
        let finished = done.clone();
        std::thread::spawn(move || {
            std::mem::forget(reserve_discord_handoff("codex", TMUX, ORIGIN));
            let evidence = record(Some("t1"), Some(Utc::now()));
            let outcome = bind_discord_handoff_record(TMUX, ORIGIN, &evidence);
            finished.store(true, Ordering::SeqCst);
            let _ = bound_tx.send(outcome);
        });
        std::thread::sleep(Duration::from_millis(300));
        let _ = during_tx.send(done.load(Ordering::SeqCst));
    });
    assert_eq!(hook(INPUT, "t1"), PromptObservation::PublishedSshDirect);
    let bound_during = during_rx.recv_timeout(Duration::from_secs(5)).expect("probe ran");
    assert!(!bound_during, "no bind lands inside the owning decision");
    let outcome = bound_rx.recv_timeout(Duration::from_secs(5)).expect("bind ran");
    assert_eq!(outcome, BindOutcome::Bound);
    assert_eq!(slot("t1"), Some(NativeTurnOwner::Relay), "the input keeps its turn");
    assert_eq!(rx.of(INPUT), vec![(false, None)]);
}

/// A hook that names no turn stays held after the bind until its record names one: an input
/// of the Discord turn is only echoed, one of a later turn owns it.
#[test]
fn a_held_input_naming_no_turn_waits_for_its_record() {
    for (turn, echo) in [("t1", true), ("t2", false)] {
        let _lock = locked();
        let mut rx = Published::new();
        let _guard = reserve_discord_handoff("codex", TMUX, ORIGIN);
        let unnamed = observe_hook_prompt_by_tmux_with_prompt_id_at("codex", TMUX, INPUT, None, Utc::now());
        assert_eq!(unnamed, PromptObservation::ParkedForHandoff);
        let origin = record(Some("t1"), Some(Utc::now()));
        assert_eq!(bind_discord_handoff_record(TMUX, ORIGIN, &origin), BindOutcome::Bound);
        assert!(rx.of(INPUT).is_empty(), "held until its record names a turn");
        let own = record(Some(turn), Some(Utc::now()));
        observe_codex_record_prompt_at(TMUX, INPUT, Some("b"), None, &own, Utc::now());
        assert_eq!(rx.of(INPUT), vec![(echo, Some("b".to_string()))], "{turn}");
        assert_eq!(held_handoff_inputs_for_tests("codex", TMUX), Some(0));
    }
}

/// A record of the origin text older than the reservation is suppressed without binding;
/// the submission's own record binds.
#[test]
fn an_older_record_of_the_origin_text_leaves_the_handoff_unbound() {
    let _lock = locked();
    let _guard = reserve_discord_handoff("codex", TMUX, ORIGIN);
    assert_eq!(hook(INPUT, "t1"), PromptObservation::ParkedForHandoff);
    let older = record(Some("t0"), Some(Utc::now() - chrono::TimeDelta::minutes(5)));
    let seen = observe_codex_record_prompt_at(TMUX, ORIGIN, Some("a0"), None, &older, Utc::now());
    assert_eq!(seen, PromptObservation::SuppressedDiscordDuplicate);
    assert_eq!(slot("t0"), None);
    assert_eq!(held_handoff_inputs_for_tests("codex", TMUX), Some(1));
    let own = record(Some("t1"), Some(Utc::now()));
    let seen = observe_codex_record_prompt_at(TMUX, ORIGIN, Some("a1"), None, &own, Utc::now());
    assert_eq!(seen, PromptObservation::SuppressedDiscordDuplicate);
    assert_eq!(slot("t1"), Some(NativeTurnOwner::Answer));
}
