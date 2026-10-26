use super::*;
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// A pause point around the owning decision.
type Probe = Arc<dyn Fn(&str) + Send + Sync>;
static BEFORE_COMMIT: Mutex<Option<Probe>> = Mutex::new(None);
static BEFORE_PARK: Mutex<Option<Probe>> = Mutex::new(None);
static BEFORE_PUBLISH: Mutex<Option<Probe>> = Mutex::new(None);

fn installed(slot: &Mutex<Option<Probe>>) -> Option<Probe> {
    slot.lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

/// Runs under the STATE lock, after the duplicate checks and before the owning commit.
pub(crate) fn before_owning_commit(prompt: &str) {
    if let Some(probe) = installed(&BEFORE_COMMIT) {
        probe(prompt);
    }
}

/// Runs under the STATE lock before an observation can join the parked inputs.
pub(crate) fn before_parking_commit(prompt: &str) {
    if let Some(probe) = installed(&BEFORE_PARK) {
        probe(prompt);
    }
}

/// Runs after the decision released the lock and before the owning event is published.
pub(crate) fn before_owning_publish(prompt: &str) {
    if let Some(probe) = installed(&BEFORE_PUBLISH) {
        probe(prompt);
    }
}

pub(crate) struct ProbeGuard(&'static Mutex<Option<Probe>>);

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = None;
    }
}

fn install(slot: &'static Mutex<Option<Probe>>, probe: Probe) -> ProbeGuard {
    *slot.lock().unwrap_or_else(|error| error.into_inner()) = Some(probe);
    ProbeGuard(slot)
}

pub(crate) fn on_owning_commit(probe: impl Fn(&str) + Send + Sync + 'static) -> ProbeGuard {
    install(&BEFORE_COMMIT, Arc::new(probe))
}

pub(crate) fn on_owning_publish(probe: impl Fn(&str) + Send + Sync + 'static) -> ProbeGuard {
    install(&BEFORE_PUBLISH, Arc::new(probe))
}

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
        Self {
            rx,
            seen: Vec::new(),
        }
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

fn competing_bind(
    reserve: bool,
) -> (
    Probe,
    mpsc::Receiver<(bool, BindOutcome)>,
    std::thread::JoinHandle<()>,
) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (attempted_tx, attempted_rx) = mpsc::channel();
    let (bound_tx, bound_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("decision probe entered");
        let blocked = matches!(STATE.try_lock(), Err(std::sync::TryLockError::WouldBlock));
        attempted_tx.send(()).unwrap();
        if reserve {
            std::mem::forget(reserve_discord_handoff("codex", TMUX, ORIGIN));
        }
        let evidence = record(Some("t1"), Some(Utc::now()));
        let outcome = bind_discord_handoff_record(TMUX, ORIGIN, &evidence);
        bound_tx.send((blocked, outcome)).unwrap();
    });
    let entered_tx = Mutex::new(Some(entered_tx));
    let attempted_rx = Mutex::new(attempted_rx);
    let probe = Arc::new(move |prompt: &str| {
        if prompt != INPUT {
            return;
        }
        let Some(entered_tx) = entered_tx.lock().unwrap().take() else {
            return;
        };
        entered_tx.send(()).unwrap();
        attempted_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .expect("binder attempted STATE acquisition during the decision");
    });
    (probe, bound_rx, worker)
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
    let seen =
        |id| observe_codex_prompt_in_turn_at(TMUX, "second input", Some(id), Some("t2"), now);
    assert_eq!(seen("e1"), PromptObservation::ParkedForHandoff);
    assert_eq!(seen("e2"), PromptObservation::ParkedForHandoff);
    assert_eq!(held_handoff_inputs_for_tests("codex", TMUX), Some(1));
    drop(guard);
    assert_eq!(rx.of("second input"), vec![(false, Some("e1".to_string()))]);
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
    assert_eq!(
        hook("next turn input", "t2"),
        PromptObservation::ParkedForHandoff
    );
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

fn restored_remaining_ttl(legacy_snapshot: bool) {
    let _env = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let _lock = locked();
    let mut rx = Published::new();
    std::mem::forget(reserve_discord_handoff("codex", TMUX, ORIGIN));
    assert_eq!(hook(INPUT, "t1"), PromptObservation::ParkedForHandoff);
    Instant::advance(Duration::from_secs(230 * 60));
    sync_durable("codex", TMUX);
    assert!(durable_handoff_present_for_tests("codex", TMUX));
    if legacy_snapshot {
        let path = durable_path("codex", TMUX).unwrap();
        let mut snapshot: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        snapshot.as_object_mut().unwrap().remove("expires_at");
        snapshot["reservation"]["reserved_wall"] =
            serde_json::json!(Utc::now() - chrono::TimeDelta::minutes(230));
        std::fs::write(path, serde_json::to_string(&snapshot).unwrap()).unwrap();
    }
    reset_state_for_tests();
    // The restarted process runs on a host already up longer than the handoff's age.
    Instant::advance(PROMPT_ANCHOR_SUBMIT_TTL);
    let rollout = root.path().join("rollout.jsonl");
    std::fs::write(&rollout, "").unwrap();
    codex_rehydrated_scan_offset(TMUX, &rollout);
    assert_eq!(held_handoff_inputs_for_tests("codex", TMUX), Some(1));
    drain_expired_handoffs();
    assert!(
        rx.of(INPUT).is_empty(),
        "the remaining ten minutes are preserved"
    );
    Instant::advance(Duration::from_secs(11 * 60));
    drain_expired_handoffs();
    drain_expired_handoffs();
    assert_eq!(
        rx.of(INPUT),
        vec![(false, None)],
        "the original four-hour TTL drains once"
    );
    assert_eq!(held_handoff_inputs_for_tests("codex", TMUX), None);
    assert!(!durable_handoff_present_for_tests("codex", TMUX));
}

#[test]
fn a_restored_handoff_keeps_only_its_remaining_ttl() {
    restored_remaining_ttl(false);
}

#[test]
fn an_old_handoff_snapshot_keeps_its_original_expiry() {
    restored_remaining_ttl(true);
}

/// A handoff reserved and bound while an input's owning decision is in progress waits for
/// that decision: the input owns its turn and the bind leaves its Relay slot in place.
#[test]
fn a_bind_waits_for_an_owning_decision_in_progress() {
    let _lock = locked();
    let mut rx = Published::new();
    let (probe, bound_rx, worker) = competing_bind(true);
    let _probe = install(&BEFORE_COMMIT, probe);
    let observation = hook(INPUT, "t1");
    let result = bound_rx.recv_timeout(Duration::from_secs(5));
    let joined = worker.join();
    let (blocked, outcome) = result.expect("bind ran");
    joined.expect("binder finished");
    assert!(
        blocked,
        "STATE acquisition is blocked inside the owning decision"
    );
    assert_eq!(observation, PromptObservation::PublishedSshDirect);
    assert_eq!(outcome, BindOutcome::Bound);
    assert_eq!(
        slot("t1"),
        Some(NativeTurnOwner::Relay),
        "the input keeps its turn"
    );
    assert_eq!(rx.of(INPUT), vec![(false, None)]);
}

/// A bind cannot settle a reservation until the concurrent observation has parked its copy.
#[test]
fn a_bind_waits_for_a_parking_decision_in_progress() {
    let _lock = locked();
    let mut rx = Published::new();
    let _guard = reserve_discord_handoff("codex", TMUX, ORIGIN);
    let (probe, bound_rx, worker) = competing_bind(false);
    let _probe = install(&BEFORE_PARK, probe);
    let observation = hook(INPUT, "t1");
    let result = bound_rx.recv_timeout(Duration::from_secs(5));
    let joined = worker.join();
    let (blocked, outcome) = result.expect("bind ran");
    joined.expect("binder finished");
    assert!(
        blocked,
        "STATE acquisition is blocked inside the parking decision"
    );
    assert_eq!(observation, PromptObservation::ParkedForHandoff);
    assert_eq!(outcome, BindOutcome::Bound);
    assert_eq!(slot("t1"), Some(NativeTurnOwner::Answer));
    assert_eq!(held_handoff_inputs_for_tests("codex", TMUX), Some(0));
    assert_eq!(rx.of(INPUT), vec![(true, None)]);
}

/// A hook that names no turn stays held after the bind until its record names one: an input
/// of the Discord turn is only echoed, one of a later turn owns it.
#[test]
fn a_held_input_naming_no_turn_waits_for_its_record() {
    for (turn, echo) in [("t1", true), ("t2", false)] {
        let _lock = locked();
        let mut rx = Published::new();
        let _guard = reserve_discord_handoff("codex", TMUX, ORIGIN);
        let unnamed =
            observe_hook_prompt_by_tmux_with_prompt_id_at("codex", TMUX, INPUT, None, Utc::now());
        assert_eq!(unnamed, PromptObservation::ParkedForHandoff);
        let origin = record(Some("t1"), Some(Utc::now()));
        assert_eq!(
            bind_discord_handoff_record(TMUX, ORIGIN, &origin),
            BindOutcome::Bound
        );
        assert!(
            rx.of(INPUT).is_empty(),
            "held until its record names a turn"
        );
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
