use super::injected_steer::INJECTED_STEER_TTL;
use super::*;

/// Moves the session's ledger entries past their lifetime; other state is never reset, since
/// tests outside this lock keep runtime bindings in it.
fn expire_ledger(tmux: &str) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    let key = PromptKey::new("codex", tmux);
    for entry in state
        .injected_steer_by_tmux
        .get_mut(&key)
        .into_iter()
        .flatten()
    {
        entry.recorded_at = Instant::now() - INJECTED_STEER_TTL - Duration::from_secs(1);
    }
}

fn frame(nonce: &str) -> String {
    format!("[📱 discord · ann · {nonce}]\nsame words every time")
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
    // Without the ledger the same record opens a direct-input turn.
    let elsewhere = "AgentDesk-codex-injected-join-unknown";
    let unknown = observe(elsewhere, &frame("aaaa0000"), None, Some("t1"));
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

/// The ledger refuses a ninth unsettled input rather than evict, keeps the newest settled results,
/// and after its lifetime lets the input take the ordinary path, still delivered.
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
    expire_ledger(tmux);
    let late = observe(tmux, &frame("f0000001"), None, Some("t9"));
    let direct = Observed {
        observation: PromptObservation::PublishedSshDirect,
        published: 1,
    };
    assert_eq!(late, direct);
}
