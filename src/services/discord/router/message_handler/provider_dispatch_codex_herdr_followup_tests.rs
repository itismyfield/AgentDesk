//! A Bound Codex Herdr pane's later prompts and its restart: each prompt is confirmed before one
//! write and a refused one writes nothing; a restart restores only the source its log names.

use super::*;
use crate::services::discord::recovery_engine::herdr_reader::{
    ReconnectCounts, reconnect_counts, reconnect_restarted_herdr_panes,
};

/// A ready screen whose boxed composer holds the `body` lines.
fn composer(body: &str) -> String {
    let edge = "─".repeat(30);
    format!(
        "earlier output\n╭{edge}╮\n{body}\n╰{edge}╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send"
    )
}

const SIGN_IN: &str = "Welcome to Codex\n\n  Sign in with ChatGPT to use Codex as part of your plan\n\n\
> 1. Sign in with ChatGPT\n  2. Provide your own API key\n\n  Press Enter to continue";

/// A cold start answered and bound; its nonce and rollout.
fn launch(fx: &Fixture, ports: &Ports<'_>) -> (String, PathBuf) {
    let launched = Mutex::new(None);
    let (result, _) = fx.turn(&HostedRecord::Legacy, ports, || {
        if let Some(nonce) = fx.start_provider(&ports.launcher, true) {
            *launched.lock().unwrap() = fx.answer(&nonce).map(|(_, path)| (nonce, path));
        }
    });
    assert_eq!(result, Ok(()));
    assert_eq!(fx.row(), Some(HostedState::Bound));
    launched.into_inner().unwrap().unwrap()
}

/// What a dcserver restart forgets, then `provider`'s restart pass on this node's endpoint; the
/// fixture's hook ingress holds the dedupe test lock, so no other dedupe test sees the reset.
fn restart_and_reconnect(fx: &Fixture, provider: &ProviderKind) -> ReconnectCounts {
    crate::services::tui_prompt_dedupe::reset_state_for_tests();
    crate::services::tui_prompt_dedupe::binding_events::forget_channel_for_tests(CHANNEL);
    reconnect(fx, provider)
}

fn reconnect(fx: &Fixture, provider: &ProviderKind) -> ReconnectCounts {
    let _runtime = fx.rt.enter();
    let _registry = fx.rig.registry_on_this_thread();
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[]);
    fx.rig.show_panes(&[PANE]);
    reconnect_restarted_herdr_panes(Some(&fx.pool), provider);
    reconnect_counts()
}

fn counts(published: usize, withheld: usize) -> ReconnectCounts {
    ReconnectCounts {
        channels: published + withheld,
        published,
        withheld,
        ..ReconnectCounts::default()
    }
}

fn binding(fx: &Fixture) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical())
}

// T2-2: composer text on either side of the cursor, or an unread composer, refuses the follow-up
// with nothing written, cleared or held and the pane Bound; an empty one then takes it.
#[test]
fn a_draft_in_the_bound_composer_refuses_the_follow_up_and_is_left_as_it_is_pg() {
    let fx = Fixture::admitted("draft");
    let launcher = Arc::new(Launcher::default());
    let (nonce, path) = launch(&fx, &fx.ports(&launcher));
    for (body, why) in [
        ("│ 남은 초안▌                   │", "ComposerDraft"),
        ("│ ▌남은 초안                   │", "ComposerDraft"),
        (
            "│ 남은 초안                    │\n│ ▌                            │",
            "ComposerDraft",
        ),
        ("│                              │", "ComposerUnread"),
    ] {
        fx.rig.answer("pane.read", screen(&composer(body)));
        let (second, _) = fx.turn(&fx.record(), &fx.ports(&launcher), || {});
        assert_eq!(
            fx.rig.sends(),
            prompt_sends(),
            "no write and no key: {body}"
        );
        assert!(!hold_of(&nonce).exists(), "{body}");
        assert_eq!(fx.row(), Some(HostedState::Bound), "{body}");
        let second = second.unwrap_err();
        assert!(second.contains(why), "{body}: {second}");
    }
    fx.rig.answer(
        "pane.read",
        screen(&composer("│ ▌                            │")),
    );
    let (third, messages) = fx.turn(&fx.record(), &fx.ports(&launcher), || {
        reply(&fx, &path, 4, "t2", "둘째")
    });
    assert_eq!(third, Ok(()));
    assert_eq!(texts(&messages), ["둘째"]);
}

// T2-3: a follow-up whose launch options are not the pane's, whose kept options name another
// execution or none, or whose pane kept none, writes nothing.
#[test]
fn changed_or_unkept_launch_options_refuse_the_follow_up_pg() {
    let fx = Fixture::admitted("options");
    let launcher = Arc::new(Launcher::default());
    let (nonce, _) = launch(&fx, &fx.ports(&launcher));
    let kept = options_of(&fx);
    let launched = std::fs::read_to_string(&kept).unwrap();
    let (_, fingerprint) = launched.trim().split_once(' ').unwrap();
    let other = format!("{} {fingerprint}", uuid::Uuid::new_v4());
    for options in [
        Some(other.as_str()),
        Some(fingerprint),
        Some("launched with another model"),
        None,
    ] {
        match options {
            Some(options) => std::fs::write(&kept, options).unwrap(),
            None => std::fs::remove_file(&kept).unwrap(),
        }
        let (second, _) = fx.turn(&fx.record(), &fx.ports(&launcher), || {});
        let second = second.unwrap_err();
        assert!(
            second.contains("LaunchOptionsChanged"),
            "{options:?}: {second}"
        );
        assert_eq!(fx.rig.sends(), prompt_sends(), "{options:?}");
        assert!(!hold_of(&nonce).exists());
    }
}

// The contract on an unclear follow-up write: its hold stays, nothing is sent again, and the
// next prompt is held.
#[test]
fn an_unclear_follow_up_keeps_its_hold_and_the_next_prompt_writes_nothing_pg() {
    let fx = Fixture::admitted("unclear-follow-up");
    let launcher = Arc::new(Launcher::default());
    let (nonce, _) = launch(&fx, &fx.ports(&launcher));
    fx.rig.leave_sends_unanswered(true);
    let (second, _) = fx.turn(&fx.record(), &fx.ports(&launcher), || {});
    assert!(second.is_err());
    let text_only = [prompt_sends(), prompt_sends()[..1].to_vec()].concat();
    held_after(&fx, &launcher, &nonce, text_only);
}

// A cancel after the follow-up's text landed and before its Enter keeps the hold, since that text
// may sit in the composer; no Enter follows and the next prompt is held.
#[test]
fn a_cancel_between_the_follow_up_text_and_its_enter_keeps_the_hold_pg() {
    let fx = Fixture::admitted("cancel-before-enter");
    let launcher = Arc::new(Launcher::default());
    let (nonce, _) = launch(&fx, &fx.ports(&launcher));
    let (second, _) = fx.turn(&fx.record(), &fx.ports(&launcher), || {
        if wait_for(&fx.finished, "the follow-up text", || {
            fx.rig.sends().len() == 3
        }) {
            fx.cancel_now();
        }
    });
    assert!(second.unwrap_err().contains("cancel"));
    let text_only = [prompt_sends(), prompt_sends()[..1].to_vec()].concat();
    held_after(&fx, &launcher, &nonce, text_only);
}

// An Enter whose reply never came may have submitted the follow-up: its hold stays and nothing is
// sent again.
#[test]
fn an_unanswered_follow_up_enter_keeps_the_hold_and_sends_nothing_again_pg() {
    let fx = Fixture::admitted("unclear-enter");
    let launcher = Arc::new(Launcher::default());
    let (nonce, _) = launch(&fx, &fx.ports(&launcher));
    let (second, _) = fx.turn(&fx.record(), &fx.ports(&launcher), || {
        if wait_for(&fx.finished, "the follow-up text", || {
            fx.rig.sends().len() == 3
        }) {
            // The text is answered at once; its Enter waits out a 200ms settle first.
            std::thread::sleep(Duration::from_millis(60));
            fx.rig.leave_sends_unanswered(true);
        }
    });
    assert!(second.is_err());
    held_after(
        &fx,
        &launcher,
        &nonce,
        [prompt_sends(), prompt_sends()].concat(),
    );
}

/// A follow-up that left exactly `sent` keeps its hold, and the next prompt writes nothing.
fn held_after(fx: &Fixture, launcher: &Arc<Launcher>, nonce: &str, sent: Vec<Value>) {
    assert_eq!(fx.rig.sends(), sent);
    assert!(hold_of(nonce).exists());
    fx.rig.leave_sends_unanswered(false);
    let (third, _) = fx.turn(&fx.record(), &fx.ports(launcher), || {});
    assert!(third.unwrap_err().contains("input held"));
    assert_eq!(fx.rig.sends(), sent);
}

// T2-4: after a restart only the Codex pass reads a Codex row, while Codex turns run on Herdr; it
// restores the logged rollout from its end, and the next prompt is written and read once.
#[test]
fn a_restart_restores_the_logged_rollout_of_a_matched_codex_pane_for_its_next_prompt_pg() {
    let fx = Fixture::admitted("reconnect");
    let launcher = Arc::new(Launcher::default());
    let (nonce, path) = launch(&fx, &fx.ports(&launcher));
    assert_eq!(
        restart_and_reconnect(&fx, &ProviderKind::Claude),
        counts(0, 0)
    );
    assert_eq!(binding(&fx), None, "the Claude pass reads no Codex row");
    {
        let _off = crate::services::turn_host::force_codex_switch_for_test(Some(false));
        assert_eq!(reconnect(&fx, &ProviderKind::Codex), counts(0, 0));
    }
    assert_eq!(binding(&fx), None, "off, the Codex pass reads nothing");
    let (unbound, _) = fx.turn(&fx.record(), &fx.ports(&launcher), || {});
    assert!(unbound.unwrap_err().contains("NoSource"));
    assert_eq!(fx.rig.sends(), prompt_sends());
    let _on = crate::services::turn_host::force_codex_switch_for_test(Some(true));
    assert_eq!(reconnect(&fx, &ProviderKind::Codex), counts(1, 0));
    assert_eq!(
        reconnect(&fx, &ProviderKind::Claude),
        counts(1, 0),
        "each pass keeps the other's"
    );
    let restored = binding(&fx).unwrap();
    use crate::services::agent_protocol::RuntimeHandoffKind;
    assert_eq!(restored.runtime_kind, RuntimeHandoffKind::CodexTui);
    let rollout = std::fs::metadata(&path).unwrap().len();
    assert_eq!(
        (Path::new(&restored.output_path), restored.last_offset),
        (path.as_path(), rollout)
    );
    let (second, messages) = fx.turn(&fx.record(), &fx.ports(&launcher), || {
        reply(&fx, &path, 4, "t2", "둘째")
    });
    assert_eq!(second, Ok(()));
    assert_eq!(texts(&messages), ["둘째"]);
    assert_eq!(fx.rig.sends(), [prompt_sends(), prompt_sends()].concat());
    assert!(!hold_of(&nonce).exists());
    assert_eq!(
        launcher.creates.load(Ordering::SeqCst),
        1,
        "nothing relaunched"
    );
}

// T2-5: a pane whose root shell was started again after its launch takes no follow-up, attached
// or restored after a restart; nothing is written, held or launched.
#[test]
fn a_replaced_root_shell_restores_no_codex_source_and_takes_no_input_pg() {
    let fx = Fixture::admitted("root-replaced");
    let launcher = Arc::new(Launcher::default());
    let (nonce, _) = launch(&fx, &fx.ports(&launcher));
    fx.rig.restart_shell(&context_of(&nonce));
    let (attached, _) = fx.turn(&fx.record(), &fx.ports(&launcher), || {});
    assert!(attached.unwrap_err().contains("not confirmed"));
    assert_eq!(fx.rig.sends(), prompt_sends());
    assert!(!hold_of(&nonce).exists());
    let _on = crate::services::turn_host::force_codex_switch_for_test(Some(true));
    assert_eq!(
        restart_and_reconnect(&fx, &ProviderKind::Codex),
        counts(0, 1)
    );
    assert_eq!(binding(&fx), None);
    let (second, _) = fx.turn(&fx.record(), &fx.ports(&launcher), || {});
    assert!(second.is_err());
    assert_eq!(fx.rig.sends(), prompt_sends());
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
}

/// A bound pane whose log `change` leaves without its logged source restores nothing on restart,
/// and its next prompt is refused with nothing written.
fn restart_restores_nothing(tag: &str, change: impl FnOnce(&Fixture, &Path)) {
    let _on = crate::services::turn_host::force_codex_switch_for_test(Some(true));
    let fx = Fixture::admitted(tag);
    let launcher = Arc::new(Launcher::default());
    let (_, path) = launch(&fx, &fx.ports(&launcher));
    change(&fx, &path);
    assert_eq!(
        restart_and_reconnect(&fx, &ProviderKind::Codex),
        counts(0, 1)
    );
    assert_eq!(binding(&fx), None);
    let (second, _) = fx.turn(&fx.record(), &fx.ports(&launcher), || {});
    assert!(second.unwrap_err().contains("NoSource"));
    assert_eq!(fx.rig.sends(), prompt_sends());
}

// A rollout file replaced since its hook logged it is not the logged source.
#[test]
fn a_restart_restores_no_codex_rollout_replaced_since_it_was_logged_pg() {
    restart_restores_nothing("replaced-rollout", |_, path| {
        let text = std::fs::read(path).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::write(path, text).unwrap();
    });
}

// A later Pending of the same execution leaves no baseline; Codex has no clear to wait on.
#[test]
fn a_restart_restores_no_codex_source_behind_a_later_pending_pg() {
    use crate::services::tui_prompt_dedupe::binding_events::{
        self, BindingCause, CauseSource, HookSignal, Proposal,
    };
    restart_restores_nothing("later-pending", |fx, path| {
        let text = path.display().to_string();
        let payload = json!({"source": "clear", "session_id": "later", "transcript_path": text});
        let hook = HookSignal::from_payload("session_start", &payload);
        let proposal = Proposal {
            channel_id: CHANNEL,
            provider: "codex",
            tmux_session: fx.logical(),
            session_id: Some("later"),
            path: &text,
            replaced: None,
            cause: CauseSource::Hook(BindingCause::Clear),
            hook: Some(&hook),
        };
        crate::services::tmux_common::with_tmux_source_authority(fx.logical(), |_| {
            binding_events::record_pending(&proposal).unwrap();
        });
    });
}

// T2-8: O posts what a turn wrote before a restart once, and after the restart and the pane's
// restore only what was written since; nothing is posted again or lost.
#[test]
fn o_posts_only_the_rest_of_a_turn_across_a_restart_and_its_restore_pg() {
    let fx = Fixture::admitted("o-restart");
    let launcher = Arc::new(Launcher::default());
    let o = Mutex::new(None);
    let mut ports = fx.ports(&launcher);
    ports.o = Some(&o);
    let (_, path) = launch(&fx, &ports);
    let mut o = o.into_inner().unwrap().unwrap();
    let step = |o: &mut ODrive| {
        let _runtime = fx.rt.enter();
        fx.rt.block_on(o.step())
    };
    assert_eq!(step(&mut o), ["답"]);
    append(&path, &started_lines("t2", "앞"));
    assert_eq!(step(&mut o), ["답", "앞"]);
    let _on = crate::services::turn_host::force_codex_switch_for_test(Some(true));
    assert_eq!(
        restart_and_reconnect(&fx, &ProviderKind::Codex),
        counts(1, 0)
    );
    o.restart();
    append(&path, &turn_lines("t2", "뒤")[1..]);
    assert_eq!(step(&mut o), ["답", "앞", "뒤"]);
    assert_eq!(step(&mut o), ["답", "앞", "뒤"]);
}

// A cold start whose composer shows a sign-in modal is refused at once, before its hold and any
// write; the Pending pane is kept.
#[test]
fn a_modal_on_a_cold_start_refuses_its_first_prompt_at_once_pg() {
    let fx = Fixture::admitted("modal");
    let launcher = Arc::new(Launcher::default());
    let started = Mutex::new(None);
    let (first, _) = fx.turn(&HostedRecord::Legacy, &fx.ports(&launcher), || {
        if fx.start_provider(&launcher, false).is_some() {
            *started.lock().unwrap() = Some(Instant::now());
            fx.rig.answer("pane.read", screen(SIGN_IN));
        }
    });
    let waited = started.into_inner().unwrap().unwrap().elapsed();
    assert!(first.unwrap_err().contains("modal"));
    assert!(waited < Duration::from_secs(2), "{waited:?}");
    assert!(fx.rig.sends().is_empty());
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    assert!(!hold_of(&nonce).exists());
    assert_eq!(fx.row(), Some(HostedState::Pending));
}

// A cold start whose composer never turns ready stops at its deadline with nothing written or
// held; the Pending pane is kept.
#[test]
fn a_composer_that_never_turns_ready_stops_the_cold_start_at_its_deadline_pg() {
    let fx = Fixture::admitted("unready");
    let launcher = Arc::new(Launcher::default());
    let (first, _) = fx.turn(&HostedRecord::Legacy, &fx.ports(&launcher), || {
        if fx.start_provider(&launcher, false).is_some() {
            fx.rig.answer("pane.read", screen("Starting Codex…"));
        }
    });
    assert!(first.unwrap_err().contains("did not become ready"));
    assert!(fx.rig.sends().is_empty());
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    assert!(!hold_of(&nonce).exists());
    assert_eq!(fx.row(), Some(HostedState::Pending));
}
