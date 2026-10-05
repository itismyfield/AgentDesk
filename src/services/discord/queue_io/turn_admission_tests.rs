//! Only an `Activated` recovery kickoff may clear the channel's `recovery_done` latch.

use super::*;
use crate::services::turn_orchestrator::RecoveryKickoffResult;

fn episode(nonce: &str) -> Arc<CancelToken> {
    Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        nonce.to_owned(),
    )))
}

async fn latched(shared: &SharedData, channel_id: ChannelId) -> bool {
    let signal = shared.mailboxes.recovery_done(channel_id);
    tokio::time::timeout(std::time::Duration::from_millis(25), signal.wait())
        .await
        .is_ok()
}

#[tokio::test]
async fn only_an_activated_kickoff_resets_the_recovery_done_latch() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel_id = ChannelId::new(5_951_201);
    let occupant = episode("a");
    assert!(
        shared
            .mailbox(channel_id)
            .try_start_turn(occupant, UserId::new(51), MessageId::new(101))
            .await
    );
    shared.mailboxes.recovery_done(channel_id).mark_done();

    let refused = mailbox_recovery_kickoff(
        &shared,
        channel_id,
        episode("b"),
        UserId::new(52),
        Some(MessageId::new(202)),
    )
    .await;

    assert_eq!(refused, RecoveryKickoffResult::OccupiedDifferentEpisode);
    assert!(
        latched(&shared, channel_id).await,
        "a refused kickoff reset the live recovery's recovery_done latch"
    );

    assert!(
        shared
            .mailbox(channel_id)
            .hard_stop()
            .await
            .removed_token
            .is_some()
    );
    let activated = mailbox_recovery_kickoff(
        &shared,
        channel_id,
        episode("b"),
        UserId::new(52),
        Some(MessageId::new(202)),
    )
    .await;

    assert_eq!(activated, RecoveryKickoffResult::Activated);
    assert!(!latched(&shared, channel_id).await);
}

/// An adopting claim is fenced on the episode it names, not on the token it
/// installs, and records the installed token as the started episode.
#[tokio::test]
async fn an_adopting_claim_is_fenced_on_the_named_episode() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel_id = ChannelId::new(5_951_203);
    let (owner, released, latest) = (UserId::new(53), MessageId::new(301), MessageId::new(302));
    let exact = |msg, nonce: &str| {
        let nonce = Some(nonce.to_owned());
        let finish =
            crate::services::discord::mailbox_finish_turn_if_matches_episode_started_before;
        finish(
            &shared,
            &shared.provider,
            channel_id,
            msg,
            nonce,
            std::time::Instant::now(),
        )
    };
    let adopt = |token: &str, msg, named: &str| {
        let (kind, named) = (ActiveTurnKind::Background, Some(named.to_owned()));
        mailbox_try_start_turn_adopting(
            &shared,
            channel_id,
            episode(token),
            owner,
            msg,
            kind,
            named,
        )
    };
    let mailbox = shared.mailbox(channel_id);
    assert!(mailbox.try_start_turn(episode("e1"), owner, released).await);
    assert!(exact(released, "e1").await.removed_token.is_some());
    let refused = adopt("fresh", released, "e1").await;
    assert!(refused.refused_released_episode && !refused.started);

    assert!(mailbox.try_start_turn(episode("e2"), owner, latest).await);
    let by_id = crate::services::discord::mailbox_finish_turn_if_matches;
    let by_id = by_id(&shared, &shared.provider, channel_id, latest).await;
    assert!(by_id.removed_token.is_some());
    let adopted = adopt("fresh2", latest, "e2").await;
    assert!(adopted.started && !adopted.refused_released_episode);
    let snapshot = mailbox.snapshot().await;
    assert_eq!(snapshot.active_turn_nonce.as_deref(), Some("fresh2"));
    assert_eq!(snapshot.active_turn_kind, ActiveTurnKind::Background);

    // Releasing the installed token ends the adopted episode as well.
    assert!(exact(latest, "fresh2").await.removed_token.is_some());
    assert!(adopt("fresh3", latest, "e2").await.refused_released_episode);
}

#[test]
fn input_fence_wrapper_posts_source_notices_and_health_even_when_http_fails() {
    use crate::services::discord::input_runtime::fence::{self, Gate, Mode};
    use crate::services::turn_orchestrator::{Intervention, InterventionMode};
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let registry = crate::services::discord::health::HealthRegistry::new();
            let baseline = crate::services::discord::health::build_health_snapshot(&registry).await;
            let baseline = serde_json::to_value(baseline).unwrap();
            let channel = ChannelId::new(6_325_301);
            let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
            let empty = crate::services::discord::health::build_health_snapshot(&registry).await;
            let empty = serde_json::to_value(empty).unwrap();
            assert_eq!(baseline["degraded_reasons"], empty["degraded_reasons"]);
            assert_eq!(baseline["status"], empty["status"]);
            let _closing = gate.close().unwrap();
            let item = || Intervention {
                author_id: UserId::new(7),
                author_is_bot: false,
                message_id: MessageId::new(61),
                source_message_ids: vec![MessageId::new(61), MessageId::new(62)],
                queued_generation: 1,
                source_message_queued_generations: Vec::new(),
                source_text_segments: Vec::new(),
                text: "private-body-must-not-leak".into(),
                mode: InterventionMode::Soft,
                created_at: std::time::Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
                voice_announcement: None,
            };
            let (log, _http) =
                super::super::super::shared_state::test_rest::recording_mock(900, channel.get())
                    .await;
            let result = mailbox_enqueue_observed_intervention(
                &shared,
                &ProviderKind::Claude,
                channel,
                item(),
                None,
            )
            .await;
            assert_eq!(
                result.refusal_reason,
                Some(EnqueueRefusalReason::InputModeFenced(Mode::Closing))
            );
            assert!(!result.enqueued);
            assert_eq!(
                log.lock()
                    .unwrap()
                    .iter()
                    .filter(|(method, _)| method == "POST")
                    .count(),
                2
            );
            assert!(
                shared.mailboxes.peek(channel).is_none(),
                "no actor minted for a refusal"
            );
            let snapshot = crate::services::discord::health::build_health_snapshot(&registry).await;
            let snapshot = serde_json::to_value(snapshot).unwrap();
            assert!(
                snapshot["degraded_reasons"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r
                        .as_str()
                        .unwrap()
                        .contains("channel=6325301 sources=[61, 62] reason=Mode(Closing)"))
            );
            assert!(
                snapshot["degraded_reasons"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|r| !r.as_str().unwrap().contains("private-body"))
            );
            // Real failing HTTP response: Notice failure must not turn refusal into success.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let http = Arc::new(
                poise::serenity_prelude::HttpBuilder::new("test-token")
                    .proxy(format!("http://{}", listener.local_addr().unwrap()))
                    .ratelimiter_disabled(true)
                    .build(),
            );
            let _failing = super::super::super::shared_state::test_rest::install(http);
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    axum::Router::new().fallback(|| async {
                        (
                            axum::http::StatusCode::FORBIDDEN,
                            axum::Json(
                                serde_json::json!({"code":50013,"message":"Missing Permissions"}),
                            ),
                        )
                    }),
                )
                .await
                .unwrap()
            });
            let failed = mailbox_enqueue_observed_intervention(
                &shared,
                &ProviderKind::Claude,
                channel,
                item(),
                None,
            )
            .await;
            assert_eq!(failed.refusal_reason, result.refusal_reason);
            assert!(!failed.enqueued);
            assert!(
                fence::health_reasons()
                    .iter()
                    .any(|r| r.contains("channel=6325301"))
            );
            server.abort();
        });
}
