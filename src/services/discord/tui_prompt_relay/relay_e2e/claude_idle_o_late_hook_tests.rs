//! A discovered Pending source must yield to its later explicit re-pin.
use super::*;
use crate::services::tui_o::store::rotation::SourceLink;

fn hook_request(f: &Fixture, cause: &str) -> impl FnOnce() + Send + 'static {
    use crate::services::claude_tui::hook_server::observation_ingress::{
        IngressOutcome, observe_binding_hook,
    };
    use crate::services::tui_prompt_dedupe::binding_context::*;
    let nonce = std::fs::read_to_string(crate::services::tmux_common::session_temp_path(
        &f.pane.tmux,
        "spawn_nonce",
    ))
    .unwrap();
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(execution_context("claude", &nonce).unwrap()),
        observed: ObservedHookProcess::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let command = f.pane.a.clone();
    let session = f.pane.b.clone();
    let payload = serde_json::json!({"hook_event_name":"SessionStart","session_id":f.pane.b,"transcript_path":f.pane.b_path(),"source":cause});
    move || {
        assert!(matches!(
            observe_binding_hook(
                "claude",
                "SessionStart",
                Some(&command),
                Some(&session),
                &payload,
                &headers
            ),
            IngressOutcome::Durable(_)
        ));
    }
}
fn hook(f: &Fixture, cause: &str) {
    hook_request(f, cause)();
}
fn link(f: &Fixture) -> SourceLink {
    f.channel()
        .rotation()
        .unwrap()
        .link(&f.b())
        .unwrap()
        .clone()
}
async fn provisional(f: &Fixture, e: &mut Engine, copied: bool) {
    let bytes = format!(
        "{}{}",
        if copied { prefix(f) } else { String::new() },
        novel(f, "N_BODY")
    );
    f.discover(&bytes).await;
    e.capture().unwrap();
    pending(f);
    assert_eq!(link(f).hot_candidate.unwrap().parent, f.a());
}
fn refined(f: &Fixture, cause: &str) {
    let b = link(f);
    assert!(
        b.hot_candidate.is_none(),
        "late {cause} must revoke automatic eligibility"
    );
    if matches!(cause, "clear" | "startup") {
        assert_eq!(b.boundary, Boundary::Owed { from: 0 });
        assert_eq!(b.parent, None);
    } else if cause == "resume" {
        pending(f);
        assert_eq!(b.parent, None);
    } else {
        assert_eq!(b.parent, Some(f.a()));
        assert_eq!(
            b.boundary,
            Boundary::Owed {
                from: prefix(f).len() as u64
            }
        );
    }
}
#[test]
fn late_clear_and_startup_replay_the_already_spooled_n_once() {
    for cause in ["clear", "startup"] {
        run(async |f| {
            f.initialize(0);
            f.gate.acquired();
            let mut e = f.engine();
            e.capture().unwrap();
            e.deliver(100).await;
            provisional(&f, &mut e, false).await;
            hook(&f, cause);
            e.capture().unwrap();
            refined(&f, cause);
            e.deliver(100).await;
            assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
            f.assert_posted("N", &novel(&f, "N_BODY"));
            drop(e);
            let mut e = f.engine();
            e.capture().unwrap();
            e.deliver(100).await;
            assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
        });
    }
}
#[test]
fn late_parentless_resume_revokes_candidate_and_keeps_n_pending() {
    run(async |f| {
        f.initialize(0);
        let mut e = f.engine();
        e.capture().unwrap();
        provisional(&f, &mut e, true).await;
        hook(&f, "resume");
        f.gate.acquired();
        e.deliver(100).await;
        e.capture().unwrap();
        refined(&f, "resume");
        e.deliver(100).await;
        drop(e);
        let mut e = f.engine();
        e.capture().unwrap();
        e.deliver(100).await;
        refined(&f, "resume");
        assert_eq!(f.posts(), ["A_BODY"]);
    });
}
#[test]
fn late_fork_and_compact_promote_a_and_use_explicit_prefix_policy() {
    for cause in ["fork", "compact"] {
        run(async |f| {
            f.initialize(0);
            let mut e = f.engine();
            e.capture().unwrap();
            let copied = prefix(&f).replace("A_BODY", "X_BODY");
            f.discover(&format!("{copied}{}", novel(&f, "N_BODY")))
                .await;
            e.capture().unwrap();
            pending(&f);
            assert!(link(&f).hot_candidate.is_some());
            hook(&f, cause);
            e.sources.follow(&mut e.writer).unwrap();
            let b = link(&f);
            assert_eq!(b.parent, Some(f.a()));
            assert_eq!(b.boundary, Boundary::Undecided);
            assert!(b.hot_candidate.is_none());
            f.gate.acquired();
            e.deliver(100).await;
            e.capture().unwrap();
            refined(&f, cause);
            e.deliver(100).await;
            assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
            f.assert_posted("N", &novel(&f, "N_BODY"));
        });
    }
}
#[test]
fn newer_explicit_hook_blocks_proof_before_follow_applies_it() {
    run(async |f| {
        f.initialize(0);
        let mut e = f.engine();
        e.capture().unwrap();
        provisional(&f, &mut e, true).await;
        hook(&f, "resume");
        f.gate.acquired();
        e.deliver(100).await;
        // Capture reads the fresh binding log while follow still has the old checkpoint.
        e.sources
            .capture(&mut e.writer, &mut e.deriver, &mut e.owed)
            .unwrap();
        pending(&f);
        assert!(link(&f).hot_candidate.is_some());
        assert!(f.channel().ledger().boundary_evidence(&f.b()).is_none());
        e.deliver(100).await;
        assert_eq!(f.posts(), ["A_BODY"]);
        e.capture().unwrap();
        refined(&f, "resume");
    });
}
#[test]
fn refinement_survives_restart_before_checkpoint_and_duplicate_hook() {
    for cause in ["clear", "resume", "fork", "compact"] {
        run(async |f| {
            f.initialize(0);
            let mut e = f.engine();
            e.capture().unwrap();
            provisional(&f, &mut e, cause != "clear").await;
            let checkpoint = f
                .root
                .join("o_store")
                .join(f.pane.channel.get().to_string())
                .join("binding_checkpoint");
            let before = std::fs::read(&checkpoint).unwrap();
            hook(&f, cause);
            e.sources.follow(&mut e.writer).unwrap();
            assert!(link(&f).hot_candidate.is_none());
            drop(e);
            // Simulate the durable boundary replacement succeeding before checkpoint replacement.
            std::fs::write(&checkpoint, before).unwrap();
            let mut e = f.engine();
            hook(&f, cause);
            f.gate.acquired();
            e.capture().unwrap();
            e.deliver(100).await;
            refined(&f, cause);
            let expected = if cause == "resume" {
                vec!["A_BODY"]
            } else {
                vec!["A_BODY", "N_BODY"]
            };
            assert_eq!(f.posts(), expected);
            if cause != "resume" {
                f.assert_posted("N", &novel(&f, "N_BODY"));
            }
            drop(e);
            let mut e = f.engine();
            e.capture().unwrap();
            e.deliver(100).await;
            assert_eq!(f.posts(), expected);
        });
    }
}

pub(super) fn fail_parent_comparison_after_a_poll(f: &Fixture, e: &mut Engine) {
    use crate::services::tui_o::shadow::{CapturedRecord, SourceId};
    use crate::services::tui_o::writer::pieces::Derived;
    use crate::services::tui_o::writer::rotation::Owe;
    struct RemoveParent<'a> {
        f: &'a Fixture,
        parent: SourceId,
        owed: &'a mut std::collections::VecDeque<Derived>,
        removed: bool,
    }
    impl Owe for RemoveParent<'_> {
        fn finish_replay(
            &mut self,
            ledger: &crate::services::tui_o::store::ledger::LedgerState,
        ) -> crate::services::tui_o::writer::ReplaySummary {
            self.owed.finish_replay(ledger)
        }
        fn owe(&mut self, source: &SourceId, _: &CapturedRecord, derived: Vec<Derived>) {
            if *source == self.parent && !self.removed {
                let path = &self.f.pane.path;
                std::fs::rename(path, path.with_extension("saved")).unwrap();
                self.removed = true;
            }
            self.owed.extend(derived);
        }
    }
    append(&f.pane.path, "{\"type\":\"mode\"}\n");
    let mut collector = RemoveParent {
        f,
        parent: f.a(),
        owed: &mut e.owed,
        removed: false,
    };
    e.sources
        .capture(&mut e.writer, &mut e.deriver, &mut collector)
        .unwrap();
    assert!(
        collector.removed,
        "A must poll successfully before the B-only comparator fails"
    );
}

#[test]
fn refinement_guards_and_continuation_event_contract_are_durable_and_idempotent() {
    use crate::services::tui_o::writer::binding::{
        BindingCause, BindingEvents, BindingLog, BindingRecord, BindingTarget,
    };
    run(async |f| {
        f.initialize(0);
        let mut e = f.engine();
        e.capture().unwrap();
        provisional(&f, &mut e, true).await;
        hook(&f, "fork");
        let expected = link(&f);
        let event = BindingLog
            .binding_events_since(f.pane.channel.get(), expected.seq)
            .unwrap()
            .pop()
            .unwrap();
        let path = f
            .root
            .join("o_store")
            .join(f.pane.channel.get().to_string())
            .join("boundary");
        let bytes = std::fs::read(&path).unwrap();
        for fault in 0..9 {
            let mut event = event.clone();
            let mut expected = expected.clone();
            match fault {
                0 => event.channel_id += 1,
                1 => event.provider = ShadowProvider::Codex,
                2 => event.tmux_session.push_str("-other"),
                3 => event.execution_nonce.push_str("-other"),
                4 => {
                    if let BindingRecord::Bound { new, .. } = &mut event.record {
                        *new = BindingTarget::Source(f.a());
                    }
                }
                5 => event.seq = expected.seq,
                6 => {
                    if let BindingRecord::Bound { cause, .. } = &mut event.record {
                        *cause = BindingCause::Unknown;
                    }
                }
                7 => expected.hot_candidate = None,
                _ => expected.committed_at += chrono::TimeDelta::seconds(1),
            }
            assert!(
                f.channel()
                    .refine_provisional_hot_link(&expected, &event)
                    .unwrap()
                    .is_none(),
                "fault {fault}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        // Continuation has no Hook wire mapping; exercise its existing O event contract directly.
        for cause in [
            BindingCause::Clear,
            BindingCause::Startup,
            BindingCause::Resume,
            BindingCause::Fork,
            BindingCause::Continuation,
            BindingCause::Compact,
        ] {
            std::fs::write(&path, &bytes).unwrap();
            let mut event = event.clone();
            if let BindingRecord::Bound { cause: c, .. } = &mut event.record {
                *c = cause;
            }
            let mut channel = f.channel();
            assert!(
                channel
                    .refine_provisional_hot_link(&expected, &event)
                    .unwrap()
                    .is_some()
            );
            let b = link(&f);
            assert!(b.hot_candidate.is_none());
            assert_eq!(b.seq, event.seq);
            match cause {
                BindingCause::Clear | BindingCause::Startup => {
                    assert_eq!(b.boundary, Boundary::Owed { from: 0 });
                    assert_eq!(b.parent, None);
                }
                BindingCause::Resume => {
                    pending(&f);
                    assert_eq!(b.parent, None);
                }
                _ => {
                    assert_eq!(b.boundary, Boundary::Undecided);
                    assert_eq!(b.parent, Some(f.a()));
                }
            }
            let after = std::fs::read(&path).unwrap();
            assert!(
                channel
                    .refine_provisional_hot_link(&expected, &event)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(std::fs::read(&path).unwrap(), after);
        }
        std::fs::write(&path, &bytes).unwrap();
        let mut bypass = f.channel().rotation().unwrap();
        bypass
            .links
            .values_mut()
            .find(|b| b.source == f.b())
            .unwrap()
            .boundary = Boundary::Owed { from: 0 };
        assert!(f.channel().write_rotation(&bypass).is_err());
        f.store
            .seed_boundary_resolved(f.pane.channel.get(), &f.b(), 0)
            .unwrap();
        assert!(
            f.channel()
                .refine_provisional_hot_link(&expected, &event)
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    });
}

#[test]
fn explicit_resume_and_automatic_resolution_share_one_committed_order() {
    use crate::services::tui_o::writer::binding::ChannelBindingLog;
    use crate::services::tui_o::writer::rotation::Sources;
    use crate::services::tui_prompt_dedupe::binding_events;
    use std::sync::mpsc;
    for point in ["snapshot", "handoff"] {
        run(async |f| {
            f.initialize(0);
            let mut e = f.engine();
            e.capture().unwrap();
            provisional(&f, &mut e, true).await;
            f.gate.acquired();
            e.deliver(100).await;
            assert_eq!(f.posts(), ["A_BODY"]);
            let hook = hook_request(&f, "resume");
            let root = f.root.clone();
            let runtime = tokio::runtime::Handle::current();
            let (entered, begin) = mpsc::channel();
            let (release, resume) = mpsc::channel();
            let mutex_held = std::thread::scope(|scope| {
                let producer = scope.spawn(move || {
                    let _runtime = runtime.enter();
                    binding_events::set_test_root(Some(&root));
                    begin.recv_timeout(Duration::from_secs(10)).unwrap();
                    let held = binding_events::admission_mutex_held_for_tests();
                    if point == "snapshot" || !held {
                        hook();
                        release.send(()).unwrap();
                    } else {
                        release.send(()).unwrap();
                        hook();
                    }
                    binding_events::set_test_root(None);
                    held
                });
                Sources::<ChannelBindingLog>::with_boundary_pause(
                    point,
                    move || {
                        entered.send(()).unwrap();
                        resume.recv_timeout(Duration::from_secs(10)).unwrap();
                    },
                    || {
                        e.sources
                            .capture(&mut e.writer, &mut e.deriver, &mut e.owed)
                    },
                )
                .unwrap();
                producer.join().unwrap()
            });
            // Match the actor order: deliver retained work before following the new Hook.
            e.deliver(100).await;
            if point == "snapshot" || !mutex_held {
                assert_eq!(f.posts(), ["A_BODY"], "Hook durable first at {point}");
                assert!(f.channel().ledger().boundary_evidence(&f.b()).is_none());
                e.capture().unwrap();
                refined(&f, "resume");
            } else {
                opened(&f, prefix(&f).len() as u64);
                assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
                f.assert_posted("N", &novel(&f, "N_BODY"));
                e.capture().unwrap();
            }
            if point == "handoff" {
                assert!(mutex_held, "resolution must run under the append mutex");
            }
            let posts = f.posts();
            f.assert_posted(
                "A",
                &record(&f.pane.a, "a1", "2026-10-01T01:00:00Z", "A_BODY", false),
            );
            drop(e);
            let mut e = f.engine();
            e.capture().unwrap();
            e.deliver(100).await;
            assert_eq!(f.posts(), posts);
            if point == "snapshot" {
                refined(&f, "resume");
            }
        });
    }
}

#[test]
fn cold_restart_opens_retained_b_without_any_new_binding_event() {
    use crate::services::tui_prompt_dedupe::binding_events;
    run(async |f| {
        f.initialize(0);
        let mut e = f.engine();
        e.capture().unwrap();
        provisional(&f, &mut e, true).await;
        assert!(f.posts().is_empty());
        let channel = f.pane.channel.get();
        let path = f
            .root
            .join(binding_events::BINDING_EVENTS_DIR)
            .join(format!("{channel}.log"));
        let bytes = std::fs::read(&path).unwrap();
        drop(e);
        binding_events::forget_channel_for_tests(channel);
        let mut e = f.engine();
        pending(&f);
        f.gate.acquired();
        e.deliver(100).await;
        assert_eq!(f.posts(), ["A_BODY"]);
        e.capture().unwrap();
        e.deliver(100).await;
        assert_eq!(
            f.posts(),
            ["A_BODY", "N_BODY"],
            "cold admission must make progress: {:?}",
            boundary(&f)
        );
        opened(&f, prefix(&f).len() as u64);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        drop(e);
        binding_events::forget_channel_for_tests(channel);
        let mut e = f.engine();
        e.capture().unwrap();
        e.deliver(100).await;
        assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
        opened(&f, prefix(&f).len() as u64);
        f.assert_posted(
            "A",
            &record(&f.pane.a, "a1", "2026-10-01T01:00:00Z", "A_BODY", false),
        );
        f.assert_posted("N", &novel(&f, "N_BODY"));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    });
}
