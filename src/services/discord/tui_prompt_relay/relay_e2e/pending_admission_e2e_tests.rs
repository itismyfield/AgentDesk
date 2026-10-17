//! Observe every reaction request, including transient Pending before a refused claim.

use super::{AbortOnDrop, CHANNEL_ID, RelayE2eHarness, discord_mock::USER_ID};
use crate::services::discord::router::message_handler::{IntakeRequest, execute_intake_turn_core};
use crate::services::discord::{self, SharedData, turn_view_reconciler as view};
use crate::services::provider::CancelToken;
use poise::serenity_prelude::{MessageId, UserId};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

const MESSAGE: u64 = 940_487_400_051_910;
const WINNER: u64 = MESSAGE + 1;
const PENDING: &str = "%E2%8F%B3";
const QUEUED: &str = "%F0%9F%93%AC";
const MERGED: &str = "%E2%9E%95";
const DEADLINE: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
struct Effect {
    method: axum::http::Method,
    path: String,
    active: Option<MessageId>,
}

pub(super) struct EffectProbe {
    shared: Weak<SharedData>,
    history: Mutex<Vec<Effect>>,
    hold_channel: AtomicBool,
    arrived: Notify,
    release: Notify,
}

impl EffectProbe {
    pub(super) async fn observe(&self, method: &axum::http::Method, path: &str) {
        if path.contains("/reactions") {
            let shared = self.shared.upgrade().expect("live fixture");
            let snapshot = shared
                .mailbox(super::serenity::ChannelId::new(CHANNEL_ID))
                .snapshot()
                .await;
            self.history.lock().unwrap().push(Effect {
                method: method.clone(),
                path: path.into(),
                active: snapshot.active_user_message_id,
            });
        }
        if method == axum::http::Method::GET
            && path == format!("/api/v10/channels/{CHANNEL_ID}")
            && self.hold_channel.swap(false, Ordering::SeqCst)
        {
            self.arrived.notify_one();
            self.release.notified().await;
        }
    }

    fn effects(&self) -> Vec<Effect> {
        self.history.lock().unwrap().clone()
    }

    fn assert_queue_only(&self, emoji: &str) {
        self.assert_no_pending();
        let history = self.effects();
        assert!(
            history.iter().all(|effect| effect.path.contains(emoji)),
            "only the authoritative queue marker: {history:?}"
        );
        assert_eq!(
            history
                .iter()
                .filter(|effect| effect.method == axum::http::Method::PUT)
                .count(),
            1,
            "one queue marker add: {history:?}"
        );
    }

    fn assert_no_pending(&self) {
        let history = self.effects();
        assert!(
            history.iter().all(|e| !e.path.contains(PENDING)),
            "refused admission must never emit Pending, even transiently: {history:?}"
        );
    }
}

fn observe(h: &RelayE2eHarness, hold_channel: bool) -> Arc<EffectProbe> {
    h.shared
        .http
        .cached_serenity_ctx
        .set(h.ctx.clone())
        .expect("install mock HTTP");
    h.shared.turn_view_reconciler.enable_test_http_reactions();
    let probe = Arc::new(EffectProbe {
        shared: Arc::downgrade(&h.shared),
        history: Mutex::new(Vec::new()),
        hold_channel: AtomicBool::new(hold_channel),
        arrived: Notify::new(),
        release: Notify::new(),
    });
    *h.mock.admission_probe.lock().unwrap() = Some(probe.clone());
    probe
}

fn intake(h: &RelayE2eHarness) -> AbortOnDrop<Result<(), discord::Error>> {
    intake_with(h, MESSAGE, "admission effect boundary", false)
}

fn intake_with(
    h: &RelayE2eHarness,
    message: u64,
    text: &str,
    merge_consecutive: bool,
) -> AbortOnDrop<Result<(), discord::Error>> {
    let (shared, http) = (h.shared.clone(), h.ctx.http.clone());
    let request = IntakeRequest {
        intake_outbox_id: None,
        channel_id: h.channel_id,
        user_msg_id: MessageId::new(message),
        source_message_ids: Vec::new(),
        busy_followup_retry_user_msg_id: MessageId::new(message),
        request_owner: UserId::new(USER_ID),
        request_owner_name: "fixture".into(),
        user_text: text.into(),
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        merge_consecutive,
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(true),
        turn_kind: discord::TurnKind::Foreground,
        preserve_on_cancel: false,
    };
    AbortOnDrop(Some(tokio::spawn(async move {
        execute_intake_turn_core(&http, &shared, "test-token", request, Vec::new())
            .await
            .map(|_| ())
    })))
}

async fn finish(task: &mut AbortOnDrop<Result<(), discord::Error>>) -> Result<(), discord::Error> {
    tokio::time::timeout(DEADLINE, task.0.as_mut().unwrap())
        .await
        .expect("intake deadline")
        .expect("intake task")
}

async fn claim(h: &RelayE2eHarness, message: u64) {
    assert!(
        h.shared
            .mailbox(h.channel_id)
            .try_start_turn(
                Arc::new(CancelToken::new()),
                UserId::new(USER_ID),
                MessageId::new(message),
            )
            .await,
        "fixture claim"
    );
}

async fn pending_start(h: &RelayE2eHarness) -> view::TurnStartAttempt {
    view::note_intake_turn_started_current_with_attempt(
        &h.shared,
        &h.ctx.http,
        h.channel_id,
        MessageId::new(MESSAGE),
        "admitted_fixture",
    )
    .await
    .attempt()
    .expect("admitted attempt")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runtime_transition_refusal_has_only_queue_effects() {
    let h = RelayE2eHarness::start().await;
    h.mock.park_first_placeholder.store(false, Ordering::SeqCst);
    let probe = observe(&h, false);
    let _transition = h
        .shared
        .session_transition_lock(h.channel_id)
        .lock_owned()
        .await;
    finish(&mut intake(&h)).await.expect("durable queue");
    assert_eq!(
        h.mailbox().await.intervention_queue[0].message_id.get(),
        MESSAGE
    );
    probe.assert_queue_only(QUEUED);
    assert!(h.provider_inputs().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn persistence_refusal_never_emits_pending() {
    let h = RelayE2eHarness::start().await;
    let probe = observe(&h, false);
    let queue_file = h
        .root
        .path()
        .join("runtime/discord_pending_queue/claude")
        .join(&h.shared.token_hash)
        .join(format!("{CHANNEL_ID}.json"));
    std::fs::create_dir_all(queue_file).unwrap();
    let _transition = h
        .shared
        .session_transition_lock(h.channel_id)
        .lock_owned()
        .await;
    assert!(
        finish(&mut intake(&h)).await.is_err(),
        "durable refusal must propagate"
    );
    probe.assert_no_pending();
    assert!(h.mailbox().await.intervention_queue.is_empty());
    assert!(h.provider_inputs().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_mailbox_claim_has_only_queue_effects() {
    let h = RelayE2eHarness::start().await;
    h.mock.park_first_placeholder.store(false, Ordering::SeqCst);
    let probe = observe(&h, true);
    let mut task = intake(&h);
    tokio::time::timeout(DEADLINE, probe.arrived.notified())
        .await
        .unwrap();
    claim(&h, WINNER).await;
    probe.release.notify_one();
    finish(&mut task).await.expect("race loser queued");
    let snapshot = h.mailbox().await;
    assert_eq!(
        snapshot.active_user_message_id,
        Some(MessageId::new(WINNER))
    );
    assert_eq!(snapshot.intervention_queue[0].message_id.get(), MESSAGE);
    probe.assert_queue_only(QUEUED);
    assert!(h.provider_inputs().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_claim_emits_one_pending_then_post_failure_releases() {
    let h = RelayE2eHarness::start().await;
    let probe = observe(&h, false);
    let arrived = h.mock.first_placeholder_arrived.notified();
    tokio::pin!(arrived);
    arrived.as_mut().enable();
    let mut task = intake(&h);
    tokio::time::timeout(DEADLINE, &mut arrived)
        .await
        .expect("placeholder reached");
    let history = probe.effects();
    let pending: Vec<_> = history
        .iter()
        .filter(|e| e.path.contains(PENDING))
        .collect();
    assert_eq!(
        pending.len(),
        1,
        "one Pending effect after admission: {history:?}"
    );
    assert_eq!(pending[0].method, axum::http::Method::PUT);
    assert_eq!(
        pending[0].active,
        Some(MessageId::new(MESSAGE)),
        "Pending precedes the mailbox claim"
    );
    h.release_held_placeholder();
    assert!(
        finish(&mut task).await.is_err(),
        "placeholder failure propagated"
    );
    assert!(
        h.mailbox().await.cancel_token.is_none(),
        "post-admission unwind releases"
    );
    assert_eq!(
        probe
            .effects()
            .iter()
            .filter(|e| e.method == axum::http::Method::PUT && e.path.contains(PENDING))
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_rollback_preserves_newly_admitted_successor_pending() {
    let h = RelayE2eHarness::start().await;
    let probe = observe(&h, false);
    claim(&h, MESSAGE).await;
    let old = pending_start(&h).await;
    h.shared
        .mailbox(h.channel_id)
        .finish_turn(discord::queue_persistence_context(
            &h.shared,
            &h.data.provider,
            h.channel_id,
        ))
        .await;
    let arrived = h.mock.first_placeholder_arrived.notified();
    tokio::pin!(arrived);
    arrived.as_mut().enable();
    let mut task = intake(&h);
    tokio::time::timeout(DEADLINE, &mut arrived)
        .await
        .expect("successor admitted");
    let before = probe.effects().len();
    view::note_intake_start_rolled_back_to_queued_current(
        &h.shared,
        h.channel_id,
        MessageId::new(MESSAGE),
        old,
        "late_old_rollback",
    )
    .await;
    assert_eq!(
        probe.effects().len(),
        before,
        "old rollback removed successor Pending: {:?}",
        probe.effects()
    );
    assert_eq!(
        h.mailbox().await.active_user_message_id,
        Some(MessageId::new(MESSAGE))
    );
    h.release_held_placeholder();
    assert!(finish(&mut task).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_closed_actor_cannot_claim_or_erase_successor_pending() {
    let h = RelayE2eHarness::start().await;
    let probe = observe(&h, false);
    let old = h.shared.mailbox(h.channel_id);
    assert_eq!(
        h.shared.mailboxes.remove_idle_entry(h.channel_id).await,
        crate::services::turn_orchestrator::registry_purge::MailboxPurgeOutcome::Removed
    );
    claim(&h, MESSAGE).await;
    pending_start(&h).await;
    let before = probe.effects().len();
    assert!(
        !old.try_start_turn(
            Arc::new(CancelToken::new()),
            UserId::new(USER_ID),
            MessageId::new(WINNER)
        )
        .await
    );
    old.finish_turn(discord::queue_persistence_context(
        &h.shared,
        &h.data.provider,
        h.channel_id,
    ))
    .await;
    assert_eq!(
        probe.effects().len(),
        before,
        "closed actor emitted an effect"
    );
    assert_eq!(
        h.mailbox().await.active_user_message_id,
        Some(MessageId::new(MESSAGE))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn synthetic_inputs_keep_their_no_discord_reaction_policy() {
    let synthetic = crate::services::tui_input::input_key::SYNTHETIC_KEY_BASE;
    let voice = discord::voice_barge_in::INTERNAL_VOICE_MESSAGE_ID_START;
    for message in [synthetic, voice] {
        let h = RelayE2eHarness::start().await;
        let probe = observe(&h, false);
        let arrived = h.mock.first_placeholder_arrived.notified();
        tokio::pin!(arrived);
        arrived.as_mut().enable();
        let mut task = intake_with(&h, message, "synthetic input", false);
        tokio::time::timeout(DEADLINE, &mut arrived)
            .await
            .expect("synthetic claim");
        assert_eq!(
            h.mailbox().await.active_user_message_id,
            Some(MessageId::new(message))
        );
        probe.assert_no_pending();
        h.release_held_placeholder();
        assert!(finish(&mut task).await.is_err());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merged_runtime_refusal_has_only_plus_effects() {
    let h = RelayE2eHarness::start().await;
    h.mock.park_first_placeholder.store(false, Ordering::SeqCst);
    let probe = observe(&h, false);
    let _transition = h
        .shared
        .session_transition_lock(h.channel_id)
        .lock_owned()
        .await;
    let mut prior =
        crate::services::turn_orchestrator::ChannelMailboxRegistry::queued_for_test(MESSAGE - 1);
    prior.author_id = UserId::new(USER_ID);
    prior.merge_consecutive = true;
    assert!(
        h.shared
            .mailbox(h.channel_id)
            .enqueue(
                prior,
                discord::queue_persistence_context(&h.shared, &h.data.provider, h.channel_id,)
            )
            .await
            .enqueued
    );
    finish(&mut intake_with(&h, MESSAGE, "merge this input", true))
        .await
        .expect("merged custody");
    let snapshot = h.mailbox().await;
    assert_eq!(snapshot.intervention_queue.len(), 1);
    assert_eq!(
        snapshot.intervention_queue[0].source_message_ids,
        [MessageId::new(MESSAGE - 1), MessageId::new(MESSAGE)]
    );
    probe.assert_queue_only(MERGED);
    assert!(h.provider_inputs().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn placeholder_release_during_arrival_notification_completes_post() {
    use std::future::Future;
    use std::task::{Context, Wake, Waker};

    struct ReleaseOnArrival(Arc<Notify>);
    impl Wake for ReleaseOnArrival {
        fn wake(self: Arc<Self>) {
            self.0.notify_waiters();
        }
    }

    let mock = super::discord_mock::DiscordMockState::new();
    let (proxy, _, server) = super::discord_mock::start(mock.clone()).await;
    let _server = AbortOnDrop(Some(server));
    let arrived = mock.first_placeholder_arrived.notified();
    tokio::pin!(arrived);
    // Release inside the arrival wakeup to exercise the earliest possible observer.
    let waker = Waker::from(Arc::new(ReleaseOnArrival(
        mock.release_first_placeholder.clone(),
    )));
    assert!(
        arrived
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );

    let response = tokio::time::timeout(
        DEADLINE,
        reqwest::Client::new()
            .post(format!("{proxy}/api/v10/channels/{CHANNEL_ID}/messages"))
            .json(&serde_json::json!({"content": "..."}))
            .send(),
    )
    .await
    .expect("placeholder release must complete the POST")
    .expect("mock POST response");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(mock.placeholder_posts.load(Ordering::SeqCst), 1);
}
