//! Moved from src/services/claude_tui/readiness tests: they drive the Discord owner effect and
//! the managed compact identity, which read Discord-layer state.

use crate::services::claude_tui::input::submission::tests::{ScriptedTmux, view, wait};
use crate::services::claude_tui::readiness::tests::compact_tests::{compact_records, pane};
use crate::services::claude_tui::readiness::tests::{Rig, after_first_look, daemon_restart};
use crate::services::discord::health::turn_deliver::inject::first_input::pg_tests::v2_cold_fixture;
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::Duration;

// A restarted idle pane appends one metadata row while the owner's input waits: no key, the
// input refused for good with its notice owed, and a restart brings nothing of it back.
#[test]
fn an_owner_input_whose_idle_transcript_moved_once_ends_refused_without_a_key() {
    use crate::services::discord::owner_effect_for_test as effect;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.assistant("old answer");
    r.terminal();
    daemon_restart(&r.session);
    let tmux = ScriptedTmux::new(&view("", false));
    tmux.set_dead(false);
    let channel = 821_701u64;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    let attempt = json!({"nonce":"moved-once","provider":"claude",
        "target":{"host":"tmux","owner":r.session,
            "root":crate::services::tmux_common::current_tmux_owner_marker(),
            "execution_nonce":r.ctx.execution_nonce},
        "source":{"kind":"pre_source","expected_session_id":null,
            "expected_transcript":null,"binding_watermark":null}});
    let queue = crate::config::runtime_root().unwrap();
    let queue = queue.join("runtime/moved-once.json");
    let fixture = v2_cold_fixture(
        &mailbox,
        queue.clone(),
        attempt,
        "one original".into(),
        channel,
    );
    runtime.block_on(fixture).unwrap();
    let (path, native) = (r.path.clone(), r.ctx.expected_native_session_id.clone());
    let moved = after_first_look(&r.session, move || {
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{}", json!({"type":"ai-title","sessionId":native})).unwrap();
    });
    let pane = crate::services::claude_tui::busy_inject::Pane::with_program(
        &r.session,
        crate::services::platform::tmux::session_identity_tests::managed_fixture_program(
            "tmux".into(),
        ),
    )
    .unwrap();
    let token = std::sync::Arc::new(crate::services::provider::CancelToken::new());
    let send = effect::idle_claude(pane, r.session.clone(), channel, token);
    let leaf: effect::Leaf = Box::new(move |frame| {
        // The budget and its windows run out on the leaf's own thread.
        let budget = Some(Duration::from_millis(900));
        crate::services::claude_tui::input::FOLLOWUP_TIMEOUT_FOR_TESTS
            .with(|slot| slot.set(budget));
        let result = send(frame);
        crate::services::claude_tui::input::FOLLOWUP_TIMEOUT_FOR_TESTS.with(|slot| slot.set(None));
        result
    });
    let result = runtime
        .block_on(effect::run(&mailbox, std::sync::Arc::new(|| 10_000), leaf))
        .unwrap();
    adk_test_support::test_support::wait::join_within(
        moved,
        adk_test_support::test_support::wait::WAIT_CAP,
    );
    assert!(
        matches!(&result, effect::Completion::NotSent(reason) if reason.contains("unproven")),
        "{result:?}"
    );
    assert!(tmux.sends().is_empty(), "no key reaches the pane");
    // The daemon restarts before the notice went out.
    drop((mailbox, registry));
    let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    let history = crate::services::tui_input::human_frame::history::v2::Registry::at(
        r.root.join("history.json"),
    );
    let recovered = crate::services::discord::owner_recovery_for_test::recover(
        &mailbox,
        (queue, format!("cold-scope-{channel}")),
        history,
        None,
        ("claude", channel, 20_000),
    );
    assert_eq!(runtime.block_on(recovered), Ok(0));
    let snapshot = runtime
        .block_on(mailbox.v2_request(|slot| slot.as_ref().unwrap().json()))
        .unwrap();
    assert!(
        snapshot["items"].as_array().unwrap().is_empty(),
        "a refused input never returns to the queue: {snapshot}"
    );
    assert!(snapshot["retained_inputs"].as_array().unwrap().is_empty());
    let refused = snapshot["refused_inputs"].as_array().unwrap();
    assert_eq!(refused.len(), 1, "{snapshot}");
    assert_eq!(
        refused[0]["notice"],
        json!({"kind":"readiness_unproven","enqueued":false}),
        "one notice is owed"
    );
    assert!(
        !snapshot.to_string().contains("one original"),
        "no body is kept: {snapshot}"
    );
}

#[test]
fn compact_usage_caller_keeps_one_pending_across_busy_completion_and_duplicate_usage() {
    usage_completion(false);
}

#[test]
fn compact_old_generation_cannot_release_a_recreated_cycle() {
    usage_completion(true);
}

fn usage_completion(recreated: bool) {
    use crate::services::{
        claude_compact_trigger, discord::ManagedCompactTurnIdentity, provider::ProviderKind,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _compact = claude_compact_trigger::STATE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let r = Rig::new();
    r.assistant("busy");
    let tmux = pane();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _enter = runtime.enter();
    let channel = 808099;
    let identity = ManagedCompactTurnIdentity::test_fixture(channel, &r.session);
    let request = || {
        claude_compact_trigger::maybe_inject_compact(
            identity.clone(),
            &ProviderKind::Claude,
            600_000,
            Some(1_000_000),
            50,
            300_000,
        )
    };
    request();
    request();
    wait("compact checked busy at least twice", || {
        fs::read_to_string(tmux.directory().join("argv"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("capture-pane"))
            .count()
            >= 2
    });
    assert!(tmux.sends().is_empty());
    r.terminal();
    wait("one compact Enter", || {
        tmux.sends().iter().filter(|key| *key == "Enter").count() == 1
    });
    request();
    assert_eq!(tmux.sends(), ["/compact", "Enter"]);
    assert!(!r.ready());
    if recreated {
        claude_compact_trigger::clear_for_tmux_for_test(&r.session);
        let before = fs::read_to_string(tmux.directory().join("argv"))
            .unwrap()
            .matches("capture-pane")
            .count();
        tmux.show(&view("", false));
        request();
        compact_records(&r, false);
        wait("new generation polled without adopting old compact", || {
            fs::read_to_string(tmux.directory().join("argv"))
                .unwrap_or_default()
                .matches("capture-pane")
                .count()
                > before
        });
        assert!(!r.ready());
        assert_eq!(tmux.sends(), ["/compact", "Enter"]);
    } else {
        claude_compact_trigger::maybe_inject_compact(
            identity.clone(),
            &ProviderKind::Claude,
            1,
            Some(1_000_000),
            50,
            300_000,
        );
        compact_records(&r, false);
        wait("submitted generation completes after usage re-arm", || {
            r.ready()
        });
        tmux.show(&view("", false));
        assert_eq!(r.send(), Ok(()));
        assert_eq!(
            tmux.sends(),
            ["/compact", "Enter", "load", "paste", "Enter"]
        );
    }
    claude_compact_trigger::clear_for_tmux_for_test(&r.session);
}
