use super::*;
use crate::services::discord::router::turn_start::IntakeRuntimeTransition;

pub(super) async fn acquire_after_redirect_or_requeue(
    runtime: (
        &Arc<serenity::http::Http>,
        &Arc<SharedData>,
        &str,
        &ProviderKind,
    ),
    channels: (ChannelId, ChannelId),
    request: (TurnKind, UserId, MessageId, &str),
    reply: (&Option<String>, bool, bool),
    pending_uploads: &mut crate::services::cluster::attachment_transfer::uploads::PendingUploads,
    requeue: (
        bool,
        &Option<String>,
        Option<crate::services::discord::turn_view_reconciler::TurnStartAttempt>,
        bool,
        (bool, super::adk_thread::Route),
    ),
    fallback_state: (Option<String>, bool, String),
) -> Result<Option<IntakeRuntimeTransition>, Error> {
    let (http, shared, token, provider) = runtime;
    let (channel_id, original_channel_id) = channels;
    let (turn_kind, request_owner, user_msg_id, user_text) = request;
    let (reply_context, has_reply_boundary, merge_consecutive) = reply;
    let (
        reply_to_user_message,
        dispatch_id_for_thread,
        turn_start_attempt,
        preserve_on_cancel,
        (queued_drain, route),
    ) = requeue;
    // Busy queued drains return their already owned uploads to the restore owner.
    // Fresh input keeps the durable tail enqueue and slow wake policy.
    let admitted = try_intake_runtime_transition_after_redirect(shared, channel_id, fallback_state)
        .await
        .ok();
    match admitted {
        Some(transition) => Ok(Some(transition)),
        None => {
            if queued_drain {
                // An owner's front stays queued and its owner looks again once this holder
                // releases; a legacy queue parks no transition waiter and keeps its backstop.
                #[cfg(unix)]
                let (waker, lock) = (shared.clone(), shared.session_transition_lock(channel_id));
                #[cfg(unix)]
                crate::services::discord::task_supervisor::spawn_observed(
                    "transition_busy_owner_wake",
                    async move {
                        let Some(mailbox) = waker.mailbox_peek(original_channel_id) else {
                            return;
                        };
                        if mailbox.v2_request(|slot| Ok(slot.is_some())).await != Ok(true) {
                            return;
                        }
                        drop(lock.lock_owned().await);
                        crate::services::discord::session_transition::wake_installed_owner(
                            &waker,
                            original_channel_id,
                        )
                        .await;
                    },
                );
                return Err(Box::new(
                    super::super::super::intake_dispatch::QueuedTransitionBusyHandoff {
                        owned_uploads: std::mem::take(pending_uploads),
                    },
                ));
            }
            tracing::warn!(
                channel_id = channel_id.get(),
                "session transition is busy; preserving intake immediately as a durable queued intervention"
            );
            // Ordinary input already owns its R1 uploads; later arrivals stay
            // in the session. Locally completable input leaves them there too.
            race_loss::handle_race_loss_enqueue(
                http,
                shared,
                token,
                provider,
                channel_id,
                original_channel_id,
                turn_kind,
                request_owner,
                user_msg_id,
                user_text,
                reply_context,
                has_reply_boundary,
                merge_consecutive,
                pending_uploads,
                reply_to_user_message,
                dispatch_id_for_thread,
                turn_start_attempt,
                preserve_on_cancel,
                race_loss::QueuedIntakeCause::SessionTransitionBusy,
                route,
            )
            .await?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pre_admission_control::{
        pre_admission_control_tests::{fixture, session_with},
        take_channel_input_state,
    };

    fn route(target: ChannelId) -> super::super::adk_thread::Route {
        super::super::adk_thread::Route {
            target,
            parent: None,
            role: None,
        }
    }
    #[tokio::test]
    #[rustfmt::skip]
    async fn runtime_transition_busy_forwards_owned_uploads_without_taking_session_state() {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
        let (shared, channel) = fixture(Some(session_with(&["U1"], true))).await;
        let (mut owned, was_cleared) = take_channel_input_state(&shared, channel).await;
        assert!(was_cleared);
        let mut core = shared.core.lock().await;
        assert!(!core.sessions[&channel].cleared, "R1 consumes the original cleared state");
        // State arriving after R1 belongs to the session, not this handoff.
        *core.sessions.get_mut(&channel).unwrap() = session_with(&["U2"], true);
        drop(core);
        let _held = shared.session_transition_lock(channel).lock_owned().await;
        let http = Arc::new(serenity::HttpBuilder::new("test-token")
            .proxy("http://127.0.0.1:1").ratelimiter_disabled(true).build());
        // S3-19b: the held transition must reach the real busy enqueue path.
        let transition = tokio::time::timeout(std::time::Duration::from_secs(5),
            acquire_after_redirect_or_requeue(
                (&http, &shared, "test-token", &ProviderKind::Codex), (channel, channel),
                (TurnKind::Foreground, UserId::new(1), MessageId::new(566_003), "A"),
                (&None, false, false), &mut owned,
                (false, &None, None, false, (false, route(channel))), (None, false, String::new()),
            )).await.expect("busy handoff must finish").expect("busy enqueue must succeed");
        assert!(transition.is_none(), "held transition must requeue, not acquire");
        let queued = crate::services::discord::mailbox_snapshot(&shared, channel).await;
        assert_eq!(queued.intervention_queue.len(), 1);
        assert_eq!(queued.intervention_queue[0].message_id, MessageId::new(566_003));
        assert_eq!(queued.intervention_queue[0].pending_uploads, ["U1"]);
        assert_eq!(owned, ["U1"]);
        let core = shared.core.lock().await;
        assert_eq!(core.sessions[&channel].pending_uploads, ["U2"]);
        assert!(core.sessions[&channel].cleared, "busy handoff must preserve cleared");
    }
    #[tokio::test]
    async fn queued_busy_returns_only_owned_vector_and_keeps_later_session_uploads() {
        let (shared, channel) = fixture(Some(session_with(&["U2"], true))).await;
        let _held = shared.session_transition_lock(channel).lock_owned().await;
        let http = Arc::new(serenity::Http::new("test-token"));
        let mut owned = vec!["U1".into(), "Q".into()];
        let result = acquire_after_redirect_or_requeue(
            (&http, &shared, "test-token", &ProviderKind::Codex),
            (channel, channel),
            (
                TurnKind::Foreground,
                UserId::new(1),
                MessageId::new(566_004),
                "A",
            ),
            (&None, false, false),
            &mut owned,
            (false, &None, None, false, (true, route(channel))),
            (None, false, String::new()),
        )
        .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("queued busy returns owned handoff"),
        };
        let handoff = error
            .downcast::<super::super::super::super::intake_dispatch::QueuedTransitionBusyHandoff>()
            .expect("typed handoff");
        assert_eq!(handoff.owned_uploads, ["U1", "Q"]);
        assert!(owned.is_empty());
        assert_eq!(
            shared.core.lock().await.sessions[&channel].pending_uploads,
            ["U2"]
        );
        assert!(
            shared
                .mailbox(channel)
                .snapshot()
                .await
                .intervention_queue
                .is_empty()
        );
        shared.mailboxes.remove_fixture_for_test(channel);
    }

    /// A queued front refused because a transition is held starts exactly once after that holder
    /// releases, whether the release lands after the drive's failed pass or inside it; never before.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn transition_busy_front_starts_once_after_the_holder_releases() {
        use crate::services::discord::queue_io::cancel_backstop_test_support as hooks;
        use crate::services::discord::queue_io::input_owner::{admission, drive};
        use crate::test_support::wait::{WAIT_CAP, await_within};
        use tokio::sync::{Mutex, mpsc::unbounded_channel as events};
        let _root = crate::config::TestRuntimeRootGuard::new();
        let queue = crate::services::discord::queue_dispatch::persistence_context;
        for (case, n) in ["after_pass", "inside_pass", "never"]
            .into_iter()
            .zip(1_u64..)
        {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_317_100 + n);
            let mailbox = shared.mailbox(channel);
            let persistence = queue(&shared, &ProviderKind::Claude, channel);
            let v2 = crate::services::turn_orchestrator::v2_queue(&persistence, channel).unwrap();
            mailbox.v2_adopt(v2.0, v2.1).await.unwrap();
            let message = 8_000_000_000_000_317_000 + n;
            let original = serde_json::from_value(serde_json::json!({"author_id":7,
                "message_id":message,"channel_id":channel.get(),"source_message_ids":[message],
                "queued_generation":1,"text":"after the clear"}))
            .unwrap();
            let admitted = admission::admit(&mailbox, &original, admission::AdmissionTicket::new());
            assert!(matches!(admitted.await, admission::Admission::Accepted(_)));
            let guard = shared.session_transition_lock(channel).lock_owned().await;
            let held = Arc::new(Mutex::new(Some(guard)));
            let (started, mut starts) = events();
            let (woke, wakes) = events();
            let wakes = Arc::new(Mutex::new(wakes));
            let (settled, mut settles) = events();
            let at = (shared.clone(), mailbox.clone(), held.clone(), wakes.clone());
            let handoff: drive::Handoff = Arc::new(move |front| {
                let ((shared, mailbox, held, wakes), started) = (at.clone(), started.clone());
                Box::pin(async move {
                    let http = Arc::new(serenity::Http::new("test-token"));
                    let id = MessageId::new(front.1.message_id);
                    let transition = acquire_after_redirect_or_requeue(
                        (&http, &shared, "test-token", &ProviderKind::Claude),
                        (channel, channel),
                        (TurnKind::Foreground, UserId::new(7), id, "after the clear"),
                        (&None, false, false),
                        &mut Vec::new(),
                        (false, &None, None, false, (true, route(channel))),
                        (None, false, String::new()),
                    )
                    .await;
                    if let Err(error) = transition {
                        // Released inside the failed pass: its wake lands while this drive runs.
                        if n == 2 && held.lock().await.take().is_some() {
                            await_within(wakes.lock().await.recv(), WAIT_CAP)
                                .await
                                .unwrap();
                        }
                        return Err(error.to_string());
                    }
                    let persistence = queue(&shared, &ProviderKind::Claude, channel);
                    let removed = mailbox.cancel_queued_primary_message(id, persistence).await;
                    assert!(removed.removed.is_some());
                    started.send(id).map_err(|e| e.to_string())
                })
            });
            let settled: drive::Settled = Arc::new(move || {
                let settled = settled.clone();
                Box::pin(async move {
                    let _ = settled.send(());
                })
            });
            let owner = Arc::new(drive::Drive {
                mailbox: mailbox.clone(),
                channel: channel.get(),
                handoff,
                settled: Some(settled),
            });
            let waking = owner.clone();
            hooks::set_owner_wake_for_tests(
                channel,
                Some(Arc::new(move || {
                    drive::wake(waking.clone());
                    let _ = woke.send(());
                })),
            );
            drive::wake(owner);
            await_within(settles.recv(), WAIT_CAP).await.unwrap();
            if n == 1 {
                assert!(starts.try_recv().is_err(), "{case}: no start while held");
                drop(held.lock().await.take());
            }
            if n == 3 {
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
                assert!(
                    starts.try_recv().is_err(),
                    "{case}: a held transition starts nothing"
                );
                assert!(
                    wakes.lock().await.try_recv().is_err(),
                    "{case}: nothing woke the owner"
                );
            } else {
                let id = await_within(starts.recv(), WAIT_CAP).await;
                assert_eq!(id, Some(MessageId::new(message)), "{case}");
                await_within(settles.recv(), WAIT_CAP).await.unwrap();
                assert!(starts.try_recv().is_err(), "{case}: the front starts once");
            }
            hooks::set_owner_wake_for_tests(channel, None);
            // The holder of the never case keeps its transition past the test.
            std::mem::forget(held.lock().await.take());
        }
    }
}
