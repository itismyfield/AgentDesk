use super::super::*;
use super::*;

const CLAUDE: &str = "claude";
const RECENT_PLUS: Duration = Duration::from_secs(31);

/// Holds the dedupe lock and a receiver subscribed before the first observation,
/// so every published `(source_event_id, prompt)` for this pane is counted.
struct Pane {
    tmux: String,
    session: String,
    rx: broadcast::Receiver<ObservedTuiPrompt>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Pane {
    fn new(tag: &str) -> Self {
        let guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmux = format!("AgentDesk-claude-prompt-id-{tag}");
        let session = format!("session-{tag}");
        runtime_binding::reset_state_for_tests();
        register_provider_session(CLAUDE, &session, &tmux);
        Self {
            rx: subscribe_observed_prompts(),
            tmux,
            session,
            _guard: guard,
        }
    }

    fn hook(&self, prompt_id: Option<&str>, prompt: &str) -> PromptObservation {
        observe_prompt_by_provider_session_with_prompt_id_at(
            CLAUDE,
            &self.session,
            prompt,
            prompt_id,
            Utc::now(),
        )
    }

    fn scan(&self, uuid: &str, prompt_id: Option<&str>, prompt: &str) -> PromptObservation {
        observe_prompt_by_tmux_with_row_ids_at(
            CLAUDE,
            &self.tmux,
            prompt,
            Some(uuid),
            prompt_id,
            Utc::now(),
        )
    }

    fn age(&self, by: Duration) {
        age_observed_prompt_records_for_tests(CLAUDE, &self.tmux, by);
    }

    /// Published events for this pane since the last call, oldest first.
    fn published(&mut self) -> Vec<(Option<String>, String)> {
        let mut events = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(event) if event.tmux_session_name == self.tmux => {
                    events.push((event.source_event_id, event.prompt));
                }
                Ok(_) => {}
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    panic!("observed-prompt receiver lagged; the count would be unsound")
                }
                Err(_) => return events,
            }
        }
    }
}

fn hook_event(prompt: &str) -> (Option<String>, String) {
    (None, prompt.to_string())
}

fn row_event(uuid: &str, prompt: &str) -> (Option<String>, String) {
    (Some(uuid.to_string()), prompt.to_string())
}

#[test]
fn hook_prompt_id_suppresses_a_scanner_replay_after_the_recent_window() {
    let mut pane = Pane::new("hook-then-scan");
    assert_eq!(
        pane.hook(Some("P"), "fix the build"),
        PromptObservation::PublishedSshDirect
    );
    pane.age(RECENT_PLUS);
    assert_eq!(
        pane.scan("U", Some("P"), "fix the build"),
        PromptObservation::SuppressedReplayedEntry
    );
    assert_eq!(pane.published(), vec![hook_event("fix the build")]);
}

#[test]
fn a_scanner_row_first_then_a_late_hook_is_not_suppressed_by_prompt_id() {
    let mut pane = Pane::new("scan-then-hook");
    assert_eq!(
        pane.scan("U", Some("P"), "run tests"),
        PromptObservation::PublishedSshDirect
    );
    pane.age(RECENT_PLUS);
    assert_eq!(
        pane.hook(Some("P"), "run tests"),
        PromptObservation::PublishedSshDirect
    );
    assert_eq!(
        pane.published(),
        vec![row_event("U", "run tests"), hook_event("run tests")]
    );
}

#[test]
fn a_prompt_id_match_also_records_the_row_uuid() {
    let mut pane = Pane::new("uuid-joins");
    pane.hook(Some("P"), "deploy");
    pane.age(RECENT_PLUS);
    assert_eq!(
        pane.scan("U", Some("P"), "deploy"),
        PromptObservation::SuppressedReplayedEntry
    );
    pane.age(RECENT_PLUS);
    assert_eq!(
        pane.scan("U", None, "deploy"),
        PromptObservation::SuppressedReplayedEntry,
        "a re-scan of the same row without promptId must match by uuid"
    );
    assert_eq!(pane.published(), vec![hook_event("deploy")]);
}

#[test]
fn a_prompt_id_seen_with_other_text_stops_suppressing_either_text() {
    let mut pane = Pane::new("ambiguous");
    pane.hook(Some("P"), "first text");
    pane.age(RECENT_PLUS);
    assert_eq!(
        pane.scan("U", Some("P"), "second text"),
        PromptObservation::PublishedSshDirect
    );
    pane.age(RECENT_PLUS);
    assert_eq!(
        pane.scan("U2", Some("P"), "first text"),
        PromptObservation::PublishedSshDirect,
        "an ambiguous prompt_id must not suppress even its first text"
    );
    assert_eq!(
        pane.published(),
        vec![
            hook_event("first text"),
            row_event("U", "second text"),
            row_event("U2", "first text"),
        ]
    );
}

#[test]
fn hooks_without_prompt_id_keep_only_the_thirty_second_content_window() {
    let mut pane = Pane::new("no-prompt-id");
    pane.hook(None, "status");
    pane.age(Duration::from_secs(29));
    assert_eq!(
        pane.scan("U", Some("P"), "status"),
        PromptObservation::SuppressedRecentDuplicate
    );
    pane.age(Duration::from_secs(2));
    assert_eq!(
        pane.scan("U2", Some("P"), "status"),
        PromptObservation::PublishedSshDirect
    );
    assert_eq!(
        pane.published(),
        vec![hook_event("status"), row_event("U2", "status")]
    );
}

#[derive(Clone, Copy, Debug)]
enum ForkCase {
    InheritedRowNeverRelayed,
    InheritedRowRelayedBefore,
    InheritedRowRelayExpired,
    HookBeforeInheritedRow,
}

/// A fork rewrites an inherited row to the fork's prompt id; with identical text
/// the new input must still be announced exactly once.
#[test]
fn fork_rewritten_prompt_ids_never_hide_the_new_input() {
    use ForkCase::*;
    for case in [
        InheritedRowNeverRelayed,
        InheritedRowRelayedBefore,
        InheritedRowRelayExpired,
        HookBeforeInheritedRow,
    ] {
        let mut pane = Pane::new(&format!("fork-{case:?}"));
        let text = "continue";
        if matches!(case, InheritedRowRelayedBefore | InheritedRowRelayExpired) {
            assert_eq!(
                pane.scan("U_old", Some("P_old"), text),
                PromptObservation::PublishedSshDirect
            );
            pane.age(match case {
                InheritedRowRelayExpired => PROMPT_ANCHOR_TTL + Duration::from_secs(1),
                _ => RECENT_PLUS,
            });
            pane.published();
        }
        if matches!(case, HookBeforeInheritedRow) {
            pane.hook(Some("Pf"), text);
            pane.age(RECENT_PLUS);
        }
        let inherited = pane.scan("U_old", Some("Pf"), text);
        pane.age(RECENT_PLUS);
        if !matches!(case, HookBeforeInheritedRow) {
            assert_eq!(
                pane.hook(Some("Pf"), text),
                PromptObservation::PublishedSshDirect,
                "{case:?}: the fork's submitted prompt must be announced"
            );
            pane.age(RECENT_PLUS);
        }
        assert_eq!(
            pane.scan("U_new", Some("Pf"), text),
            PromptObservation::SuppressedReplayedEntry,
            "{case:?}"
        );
        let expected = match case {
            InheritedRowNeverRelayed | InheritedRowRelayExpired => {
                assert_eq!(inherited, PromptObservation::PublishedSshDirect, "{case:?}");
                vec![row_event("U_old", text), hook_event(text)]
            }
            InheritedRowRelayedBefore | HookBeforeInheritedRow => {
                assert_eq!(
                    inherited,
                    PromptObservation::SuppressedReplayedEntry,
                    "{case:?}"
                );
                vec![hook_event(text)]
            }
        };
        assert_eq!(pane.published(), expected, "{case:?}");
    }
}

#[test]
fn prompt_ids_outlive_the_uuid_window_until_the_four_hour_budget() {
    for (tag, age, expected) in [
        (
            "past-uuid-window",
            PROMPT_ANCHOR_TTL + Duration::from_secs(1),
            PromptObservation::SuppressedReplayedEntry,
        ),
        (
            "inside-budget",
            PROMPT_ANCHOR_SUBMIT_TTL - Duration::from_secs(1),
            PromptObservation::SuppressedReplayedEntry,
        ),
        (
            "past-budget",
            PROMPT_ANCHOR_SUBMIT_TTL + Duration::from_secs(1),
            PromptObservation::PublishedSshDirect,
        ),
    ] {
        let mut pane = Pane::new(&format!("ttl-{tag}"));
        pane.hook(Some("P"), "long turn input");
        pane.age(age);
        assert_eq!(
            pane.scan("U", Some("P"), "long turn input"),
            expected,
            "{tag}"
        );
        let published = pane.published().len();
        let want = if expected == PromptObservation::PublishedSshDirect {
            2
        } else {
            1
        };
        assert_eq!(published, want, "{tag}");
    }
}

#[test]
fn a_repeated_hook_for_the_same_prompt_id_does_not_extend_its_lifetime() {
    let mut pane = Pane::new("no-refresh");
    pane.hook(Some("P"), "input");
    pane.age(PROMPT_ANCHOR_SUBMIT_TTL - Duration::from_secs(1));
    assert_eq!(
        pane.hook(Some("P"), "input"),
        PromptObservation::SuppressedReplayedEntry
    );
    pane.age(Duration::from_secs(2));
    assert_eq!(
        pane.scan("U", Some("P"), "input"),
        PromptObservation::PublishedSshDirect,
        "the id expires four hours after its first record"
    );
    assert_eq!(
        pane.published(),
        vec![hook_event("input"), row_event("U", "input")]
    );
}

#[test]
fn the_prompt_id_ring_keeps_the_newest_512_ids_per_pane() {
    let mut pane = Pane::new("ring-cap");
    let text = |index: usize| format!("prompt {index}");
    let id = |index: usize| format!("P{index}");
    for index in 0..RELAYED_ENTRY_ID_RING_CAP {
        pane.hook(Some(&id(index)), &text(index));
    }
    pane.age(RECENT_PLUS);
    pane.rx = subscribe_observed_prompts();
    assert_eq!(
        pane.scan("Ua", Some(&id(0)), &text(0)),
        PromptObservation::SuppressedReplayedEntry,
        "512 ids still hold the oldest"
    );
    pane.hook(Some("P512"), &text(512));
    pane.age(RECENT_PLUS);
    pane.published();
    assert_eq!(
        pane.scan("Ub", Some(&id(0)), &text(0)),
        PromptObservation::PublishedSshDirect,
        "the 513th id evicts the oldest"
    );
    assert_eq!(
        pane.scan("Uc", Some(&id(1)), &text(1)),
        PromptObservation::SuppressedReplayedEntry
    );
    assert_eq!(pane.published(), vec![row_event("Ub", &text(0))]);
}

#[test]
fn suppressed_hook_observations_do_not_record_their_prompt_id() {
    let mut pane = Pane::new("suppressed-discord");
    record_discord_originated_prompt(CLAUDE, &pane.tmux, "from discord");
    assert_eq!(
        pane.hook(Some("P"), "from discord"),
        PromptObservation::SuppressedDiscordDuplicate
    );
    pane.age(RECENT_PLUS);
    assert_eq!(
        pane.scan("U", Some("P"), "from discord"),
        PromptObservation::PublishedSshDirect
    );
    assert_eq!(pane.published(), vec![row_event("U", "from discord")]);
    drop(pane);

    let mut pane = Pane::new("suppressed-recent");
    pane.scan("U0", None, "typed twice");
    assert_eq!(
        pane.hook(Some("P"), "typed twice"),
        PromptObservation::SuppressedRecentDuplicate
    );
    pane.age(RECENT_PLUS);
    assert_eq!(
        pane.scan("U", Some("P"), "typed twice"),
        PromptObservation::PublishedSshDirect
    );
    assert_eq!(
        pane.published(),
        vec![
            row_event("U0", "typed twice"),
            row_event("U", "typed twice")
        ]
    );
}
