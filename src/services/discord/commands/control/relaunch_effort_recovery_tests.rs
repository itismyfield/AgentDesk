//! What an effort restart leaves for the launch and for a restarted server.

use super::*;
use crate::services::discord::commands::effort_restart::resume_owed;

// A server that stopped after recording a ready continuation and before queueing it admits it on
// restart under its stored ticket, once however often it is asked; one not yet ready is left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restored_intent_resumes_one_ticket_pg() {
    let test = "services::discord::commands::control::relaunch_tests::restart_tests::recovery_tests::restored_intent_resumes_one_ticket_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(45, RuntimeHandoffKind::ClaudeTui).await;
    ingress::ADOPTS
        .scope(true, async {
            resume_owed(&fx.shared, fx.channel).await;
            assert!(
                fx.owner_or_none().await.is_none(),
                "nothing owed, nothing adopted"
            );

            fx.waiting_user(101).await;
            fx.record_intent(1, false).await;
            resume_owed(&fx.shared, fx.channel).await;
            assert_eq!(continuations(&fx.owner().await), 0, "not ready, not queued");

            let ticket = fx.record_intent(2, true).await;
            let mark = crate::services::claude_tui::session::owner_first_submit;
            for _ in 0..2 {
                resume_owed(&fx.shared, fx.channel).await;
                let owner = fx.owner().await;
                assert_eq!(owner["effort_restart"]["continuation"]["ticket"], ticket);
                assert_eq!(continuations(&owner), 1, "{owner}");
                let order = queued(&owner, &[CONTINUE, "user-101"]);
                assert_eq!(order, [Some(0), Some(1)], "{owner}");
                assert!(mark(SESSION), "the resume leaves its first submit to it");
            }
            assert!(!mark("another-session"));
        })
        .await;
    tc::cleanup_session_temp_files(&fx.name);
}

impl Fixture {
    async fn owner_or_none(&self) -> Option<Value> {
        let mailbox = self.shared.mailbox_peek(self.channel)?;
        let read = mailbox.v2_request(|slot| Ok(slot.as_ref().map(|owner| owner.json())));
        read.await.unwrap().map(Result::unwrap)
    }

    /// The owner's stored intent for a turn cut on `SESSION`, as the restart records it; its ticket.
    async fn record_intent(&self, revision: u64, ready: bool) -> Value {
        let request = serde_json::json!({"operation":"stopped-restart","native_session":SESSION,
            "interrupted_episode":"cut-before-the-stop","desired_revision":revision,"ready":ready});
        let request = serde_json::from_value(request).unwrap();
        let channel = self.channel.get();
        let generation = crate::services::discord::runtime_store::process_generation();
        let mailbox = self.shared.mailbox(self.channel);
        let record = mailbox.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            owner.prepare_effort(Some(request), channel, generation)
        });
        serde_json::to_value(record.await.unwrap().continuation.unwrap().ticket).unwrap()
    }
}

impl Fixture {
    /// The not-ready intent a restart stores before it ends the pane: the cut turn's ticket and
    /// the process the pane bound.
    async fn record_prepared(&self) {
        let request = serde_json::json!({"operation":"crashed-restart","native_session":SESSION,
            "interrupted_episode":OLD_TURN,"desired_revision":1,"ready":false,
            "cut":{"tmux":self.name,"bind_nonce":BOUND}});
        let request = serde_json::from_value(request).unwrap();
        let channel = self.channel.get();
        let generation = crate::services::discord::runtime_store::process_generation();
        let mailbox = self.shared.mailbox(self.channel);
        let record = mailbox.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            owner
                .prepare_effort(Some(request), channel, generation)
                .map(drop)
        });
        record.await.unwrap();
    }

    /// Waits for the owner to hold a ready intent whose continuation it admitted.
    async fn admitted(&self) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let owner = self.owner().await;
            let ticket = owner["effort_restart"]["continuation"]["ticket"].as_str();
            let prefix = ticket.map(|ticket| format!("tk:{ticket}:"));
            let receipts = owner["admission_receipts"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let admitted = prefix.is_some_and(|prefix| {
                receipts
                    .iter()
                    .any(|r| r.as_str().is_some_and(|r| r.starts_with(&prefix)))
            });
            if owner["effort_restart"]["ready"] == true && admitted {
                return owner;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "never admitted: {owner}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

// A server stopped before the end owes nothing on restart; stopped after it, row kept or settled,
// it admits the one continuation ahead of the user and tells the turn once, never the original.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_stopped_at_any_step_owes_its_continuation_once_pg() {
    let test = "services::discord::commands::control::relaunch_tests::restart_tests::recovery_tests::a_restart_stopped_at_any_step_owes_its_continuation_once_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (n, cut) in [
        (46, "before the end"),
        (47, "row kept"),
        (48, "row settled"),
    ] {
        let fx = Fixture::new(n, RuntimeHandoffKind::ClaudeTui).await;
        ingress::ADOPTS
            .scope(true, async {
                fx.open_transcript();
                fx.waiting_user(101).await;
                if cut == "row kept" {
                    test_turn::start(&fx.shared, fx.channel, 9_001, OLD_TURN).await;
                }
                fx.record_prepared().await;
                if cut != "before the end" {
                    fx.tmux.mark_missing();
                }
                assert_eq!(fx.front().await, None, "{cut}: the barrier holds");
                for _ in 0..2 {
                    resume_owed(&fx.shared, fx.channel).await;
                }
                let owner = fx.owner().await;
                assert!(!owner["items"].to_string().contains("do the long work"));
                if cut == "before the end" {
                    assert_eq!(owner["effort_restart"]["cancelled"], true, "{owner}");
                    assert_eq!(continuations(&owner), 0, "{cut}: {owner}");
                    assert_eq!(fx.front().await, Some(101), "{cut}");
                    assert!(fx.ends().is_empty(), "{cut}: nothing ended");
                    assert_eq!(notices(&fx).await, 0, "{cut}");
                    return;
                }
                assert_eq!(owner["effort_restart"]["ready"], true, "{cut}: {owner}");
                assert_eq!(continuations(&owner), 1, "{cut}: {owner}");
                let order = queued(&owner, &[CONTINUE, "user-101"]);
                assert_eq!(order, [Some(0), Some(1)], "{cut}: {owner}");
                assert_eq!(fx.ends().len(), 1, "{cut}");
                assert_eq!(notices(&fx).await, 1, "{cut}");
            })
            .await;
        tc::cleanup_session_temp_files(&fx.name);
    }
}

// An end whose transcript record is incomplete keeps the continuation prepared and the queue
// behind it; once complete, the owner's next wake records it and admits the continuation once.
#[test]
fn an_unproven_end_waits_and_continues_on_the_next_wake_pg() {
    let test = "services::discord::commands::control::relaunch_tests::restart_tests::recovery_tests::an_unproven_end_waits_and_continues_on_the_next_wake_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    // Test builds read the binding log only on threads given its root; the drive's wake runs on
    // any worker.
    let root = crate::config::runtime_root().unwrap();
    let bind = crate::services::tui_prompt_dedupe::binding_events::set_test_root;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .on_thread_start(move || bind(Some(&root)))
        .build()
        .unwrap();
    runtime.block_on(async {
        let fx = Fixture::new(49, RuntimeHandoffKind::ClaudeTui).await;
        ingress::ADOPTS
            .scope(true, async {
                fx.busy().await;
                let complete = std::fs::read(fx.transcript()).unwrap();
                let mut torn = complete.clone();
                torn.extend_from_slice(br#"{"type":"assistant","uuid":"row-torn""#);
                std::fs::write(fx.transcript(), torn).unwrap();
                let said = fx.effort_now("high").await;
                let unproven = "종료 지점을 아직 기록하지 못했습니다";
                assert!(said.contains(unproven), "{said}");
                let owner = fx.owner().await;
                assert_eq!(owner["effort_restart"]["ready"], false, "{owner}");
                assert_eq!(continuations(&owner), 0, "{owner}");
                assert_eq!(fx.front().await, None, "the queue waits behind it");
                assert!(fx.ends().is_empty());

                std::fs::write(fx.transcript(), complete).unwrap();
                let wake = crate::services::discord::router::message_handler::owner_wake;
                wake(&fx.shared, fx.channel);
                let owner = fx.admitted().await;
                assert!(continuations(&owner) <= 1, "{owner}");
                assert!(!owner["items"].to_string().contains("do the long work"));
                assert_eq!(fx.ends().len(), 1);
                assert_eq!(notices(&fx).await, 1);
            })
            .await;
        tc::cleanup_session_temp_files(&fx.name);
    });
}

// An admission whose commit cannot be confirmed is asked again: once the store recovers, the
// owner's next wake alone admits the continuation once, ahead of the waiting user.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unconfirmed_admission_continues_on_the_next_wake_pg() {
    let test = "services::discord::commands::control::relaunch_tests::restart_tests::recovery_tests::an_unconfirmed_admission_continues_on_the_next_wake_pg";
    if !api_child(test) {
        return;
    }
    use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(50, RuntimeHandoffKind::ClaudeTui).await;
    ingress::ADOPTS
        .scope(true, async {
            fx.waiting_user(101).await;
            fx.record_intent(1, true).await;
            let queues = crate::services::discord::runtime_store::discord_pending_queue_root();
            let plan = Plan {
                fail: vec![Step::ParentSync],
                ..Default::default()
            };
            let watch = Watch::new(&queues.unwrap(), plan);
            resume_owed(&fx.shared, fx.channel).await;
            assert_eq!(continuations(&fx.owner().await), 0, "not confirmed yet");
            drop(watch);

            let wake = crate::services::discord::router::message_handler::owner_wake;
            wake(&fx.shared, fx.channel);
            let owner = fx.admitted().await;
            assert!(continuations(&owner) <= 1, "{owner}");
            let order = queued(&owner, &[CONTINUE, "user-101"]);
            assert!(order == [Some(0), Some(1)] || order[0].is_none(), "{owner}");
        })
        .await;
    tc::cleanup_session_temp_files(&fx.name);
}

// The pendings a restart's launch reflects are spent by that restart, so the continuation's turn
// start resets nothing and keeps the session; a change saved after it keeps its own pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_spends_the_pending_resets_its_launch_reflects_pg() {
    let test = "services::discord::commands::control::relaunch_tests::restart_tests::recovery_tests::a_restart_spends_the_pending_resets_its_launch_reflects_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(51, RuntimeHandoffKind::ClaudeTui).await;
    ingress::ADOPTS
        .scope(true, async {
            fx.busy().await;
            let overrides = &fx.shared.overrides;
            let key = fast_mode_reset_pending_key(fx.channel, &ProviderKind::Claude);
            let owner = fx.own(None).await;
            let _ = fx.shared.http.cached_bot_token.set(TOKEN.to_string());
            overrides
                .fast_mode_session_reset_pending
                .insert(key.clone());
            overrides.model_session_reset_pending.insert(fx.channel);
            let provider = &ProviderKind::Claude;
            let said = effort_save_notice(&fx.shared, TOKEN, fx.channel, provider, "high").await;
            fx.own(Some(owner)).await;
            assert!(said.contains("지금 다시 시작"), "{said}");
            assert!(!overrides.fast_mode_session_reset_pending.contains(&key));
            assert!(!overrides.model_session_reset_pending.contains(&fx.channel));

            let reset = fx.consume().await;
            assert!(reset.is_none(), "the continuation's start resets nothing");
            assert_eq!(fx.session_id().await.as_deref(), Some(SESSION));

            overrides.model_session_reset_pending.insert(fx.channel);
            resume_owed(&fx.shared, fx.channel).await;
            assert!(
                overrides.model_session_reset_pending.contains(&fx.channel),
                "a later change keeps its pending"
            );
        })
        .await;
    tc::cleanup_session_temp_files(&fx.name);
}
