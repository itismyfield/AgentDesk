use super::injected_steer::INJECTED_STEER_TTL;
use super::*;

/// Moves the session's ledger and recent prompts past their lifetimes; other state is never
/// reset, since tests outside this lock keep runtime bindings in it.
fn expire_ledger(tmux: &str) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    let key = PromptKey::new("codex", tmux);
    let ledger = state
        .injected_steer_by_tmux
        .get_mut(&key)
        .into_iter()
        .flatten();
    for entry in ledger {
        entry.recorded_at = Instant::now() - INJECTED_STEER_TTL - Duration::from_secs(1);
    }
    let recent = state
        .recent_observed_by_tmux
        .get_mut(&key)
        .into_iter()
        .flatten();
    for entry in recent {
        entry.recorded_at = Instant::now() - RECENT_OBSERVED_TTL - Duration::from_secs(1);
    }
}

fn frame(nonce: &str) -> String {
    frame_with(nonce, "same words every time")
}

fn frame_with(nonce: &str, body: &str) -> String {
    let frame = crate::services::tui_input::human_frame::inject;
    frame("discord:1:2", "200", "ann", nonce, body)
}

/// The frame inputs were pasted with before v1.
fn legacy_frame(nonce: &str) -> String {
    let frame = crate::services::tui_input::human_frame::legacy_inject;
    frame("discord", "200", nonce, "same words every time")
}

/// Observes `prompt` as a Codex record of `turn` and returns what was published for it.
fn observe(tmux: &str, prompt: &str, entry: Option<&str>, turn: Option<&str>) -> Observed {
    let mut rx = subscribe_observed_prompts();
    let observation = observe_codex_prompt_in_turn_at(tmux, prompt, entry, turn, Utc::now());
    let published = std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|event| event.tmux_session_name == tmux)
        .count();
    Observed {
        observation,
        published,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Observed {
    observation: PromptObservation,
    published: usize,
}

fn quiet(observation: PromptObservation) -> Observed {
    Observed {
        observation,
        published: 0,
    }
}

fn register(tmux: &str, nonce: &str, turn: &str) -> bool {
    register_injected_steer("codex", tmux, nonce, turn, Some(7))
}

/// An injected input that joined its target turn is answered by that turn's owner: no observer,
/// whatever its order or whether it names the turn, publishes an echo or opens a turn for it.
#[test]
fn an_input_joining_its_target_turn_publishes_nothing() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmux = "AgentDesk-codex-injected-join";
    // Without the ledger an older frame's record opens a direct-input turn.
    let elsewhere = "AgentDesk-codex-injected-join-unknown";
    let unknown = observe(elsewhere, &legacy_frame("aaaa0000"), None, Some("t1"));
    assert_eq!(unknown.observation, PromptObservation::PublishedSshDirect);
    assert!(register(tmux, "aaaa1111", "t1"));
    let steer = PromptObservation::InjectedSteer;
    let prompt = frame("aaaa1111");
    assert_eq!(
        observe(tmux, &prompt, None, None),
        quiet(PromptObservation::InjectedDeferred)
    );
    assert_eq!(observe(tmux, &prompt, Some("e1"), Some("t1")), quiet(steer));
    assert_eq!(observe(tmux, &prompt, None, Some("t1")), quiet(steer));
    assert_eq!(observe(tmux, &prompt, None, None), quiet(steer));
    assert!(!native_turn_is_open("codex", tmux, "t1"));
    // Once the ledger forgot it, a rescan of the same record is still known by its entry id.
    expire_ledger(tmux);
    let replayed = PromptObservation::SuppressedReplayedEntry;
    assert_eq!(
        observe(tmux, &prompt, Some("e1"), Some("t1")),
        quiet(replayed)
    );
}

/// An input that landed in another turn joins that turn when it already has an owner; otherwise
/// it takes the ordinary direct-input path exactly once, however many observers see it.
#[test]
fn an_input_in_another_turn_joins_its_owner_or_takes_the_direct_path_once() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmux = "AgentDesk-codex-injected-moved";
    assert_eq!(
        observe(tmux, "typed by a person", None, Some("t2")).published,
        1
    );
    assert!(register(tmux, "bbbb0001", "t1"));
    let joined = observe(tmux, &frame("bbbb0001"), None, Some("t2"));
    assert_eq!(joined, quiet(PromptObservation::InjectedSteer));
    assert!(register(tmux, "bbbb0002", "t1"));
    let handed = observe(tmux, &frame("bbbb0002"), None, Some("t3"));
    let direct = Observed {
        observation: PromptObservation::PublishedSshDirect,
        published: 1,
    };
    assert_eq!(handed, direct);
    let again = observe(tmux, &frame("bbbb0002"), Some("e9"), Some("t3"));
    assert_eq!(again, quiet(PromptObservation::SuppressedRecentDuplicate));
}

/// An input quoting an idle Discord header is still the injected input its nonce names: it joins
/// its target turn instead of passing for a new Discord turn's echo.
#[test]
fn an_input_quoting_an_idle_header_is_settled_by_the_ledger() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmux = "AgentDesk-codex-injected-quote";
    assert!(register(tmux, "eeee0001", "t1"));
    let prompt = frame_with("eeee0001", "see [User: bob (ID: 5)] above");
    let steer = quiet(PromptObservation::InjectedSteer);
    assert_eq!(observe(tmux, &prompt, None, Some("t1")), steer);
    assert!(!native_turn_is_open("codex", tmux, "t1"));
}

/// Inputs with the same words are told apart by their nonces: each settles by its own target.
#[test]
fn the_nonce_not_the_words_names_the_input() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmux = "AgentDesk-codex-injected-nonce";
    assert!(register(tmux, "cccc0001", "t1"));
    assert!(register(tmux, "cccc0002", "t2"));
    let steer = quiet(PromptObservation::InjectedSteer);
    assert_eq!(observe(tmux, &frame("cccc0002"), None, Some("t2")), steer);
    assert_eq!(observe(tmux, &frame("cccc0001"), None, Some("t1")), steer);
}

/// The ledger refuses a ninth unsettled input and keeps the newest settled results; once it
/// forgets, an older frame takes the ordinary path, still delivered, and a v1 header opens nothing.
#[test]
fn the_ledger_is_bounded_and_forgets_into_the_ordinary_path() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmux = "AgentDesk-codex-injected-bounds";
    for n in 0..8 {
        assert!(register(tmux, &format!("p{n:07}"), "t1"));
    }
    assert!(!register(tmux, "p9999999", "t1"));
    assert!(!register(tmux, "p0000000", "t1"));
    withdraw_injected_steer("codex", tmux, "p0000000");
    assert!(register(tmux, "p9999999", "t1"));
    let tmux = "AgentDesk-codex-injected-bounds-settled";
    for n in 0..33 {
        let nonce = format!("s{n:07}");
        assert!(register(tmux, &nonce, "t1"));
        assert_eq!(observe(tmux, &frame(&nonce), None, Some("t1")).published, 0);
        withdraw_injected_steer("codex", tmux, &nonce);
    }
    let steer = PromptObservation::InjectedSteer;
    assert_ne!(
        observe(tmux, &frame("s0000000"), None, Some("t1")).observation,
        steer
    );
    assert_eq!(
        observe(tmux, &frame("s0000001"), None, Some("t1")).observation,
        steer
    );
    let tmux = "AgentDesk-codex-injected-bounds-expired";
    assert!(register(tmux, "f0000001", "t1"));
    assert!(register(tmux, "f0000002", "t1"));
    expire_ledger(tmux);
    let late = observe(tmux, &frame("f0000001"), None, Some("t9"));
    assert_eq!(late, quiet(PromptObservation::Ignored));
    assert!(!native_turn_is_open("codex", tmux, "t9"));
    let late = observe(tmux, &legacy_frame("f0000002"), None, Some("t9"));
    let direct = Observed {
        observation: PromptObservation::PublishedSshDirect,
        published: 1,
    };
    assert_eq!(late, direct);
}

/// A v1 inject header opens a turn only when its own record reaches another unowned turn; joined,
/// unsettled, unrecorded or damaged it publishes, leases and owns nothing. Older frames: as before.
#[test]
fn an_inject_header_opens_a_turn_only_when_its_record_hands_it_over() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let damaged = frame("abcd0007").replace("mode=inject", "mode=steer");
    // (case, recorded nonce and target, prompt, observed turn, observation, published events)
    type Case = (&'static str, Option<(&'static str, &'static str)>, String);
    #[rustfmt::skip]
    let cases: [(Case, Option<&str>, PromptObservation, usize); 6] = [
        (("joined", Some(("abcd0001", "t1")), frame("abcd0001")), Some("t1"), PromptObservation::InjectedSteer, 0),
        (("deferred", Some(("abcd0002", "t1")), frame("abcd0002")), None, PromptObservation::InjectedDeferred, 0),
        (("unrecorded", None, frame("abcd0003")), Some("t1"), PromptObservation::Ignored, 0),
        (("damaged", None, damaged), Some("t1"), PromptObservation::Ignored, 0),
        (("handed", Some(("abcd0005", "t1")), frame("abcd0005")), Some("t3"), PromptObservation::PublishedSshDirect, 1),
        (("legacy", None, legacy_frame("abcd0006")), Some("t1"), PromptObservation::PublishedSshDirect, 1),
    ];
    for ((case, record, prompt), turn, observation, events) in cases {
        let tmux = format!("AgentDesk-codex-injected-owner-{case}");
        if let Some((nonce, target)) = record {
            assert!(register(&tmux, nonce, target), "{case}");
        }
        let seen = observe(&tmux, &prompt, None, turn);
        let owns = events == 1;
        assert_eq!(
            seen,
            Observed {
                observation,
                published: events
            },
            "{case}"
        );
        let leased = external_input_relay_lease_present("codex", &tmux, 1);
        assert_eq!(leased, owns, "{case}");
        let opened = turn.is_some_and(|turn| native_turn_is_open("codex", &tmux, turn));
        assert_eq!(opened, owns, "{case}");
    }
}

/// A hook may settle the input before the scanner sees its rollout row; the row's entry id is
/// still kept, so a rescan after the ledger and recent prompts expire publishes nothing.
#[test]
fn a_rollout_row_seen_after_the_hook_is_still_known_once_the_ledger_forgets() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmux = "AgentDesk-codex-injected-hook-first";
    assert!(register(tmux, "dddd0001", "t1"));
    let prompt = frame("dddd0001");
    let steer = quiet(PromptObservation::InjectedSteer);
    assert_eq!(observe(tmux, &prompt, None, Some("t1")), steer);
    assert_eq!(observe(tmux, &prompt, Some("e1"), Some("t1")), steer);
    expire_ledger(tmux);
    let replayed = quiet(PromptObservation::SuppressedReplayedEntry);
    assert_eq!(observe(tmux, &prompt, Some("e1"), Some("t1")), replayed);
}
