//! Regression coverage for terminal delivery epilogue routing.

use super::delivery_epilogue::*;
use super::*;

use std::{
    future::Future,
    io::Write,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use crate::services::discord::{formatting::ReplaceLongMessageOutcome, gateway::GatewayFuture};
use crate::services::tui_o::channel_policy::SinkOp;
use tracing_subscriber::fmt::MakeWriter;

#[cfg(all(test, unix))]
mod recovery_retry_guard_tests;
#[cfg(unix)]
mod rowless_receipt_tests;

#[tokio::test]
async fn scheduled_recovery_keeps_dispatch_unsettled_after_notice_delivery() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    for (recovery_retry, resume_failure) in [(true, false), (false, true)] {
        for (should_complete, should_fail) in [(true, false), (false, true)] {
            assert!(
                !settle_terminal_dispatch(TerminalDispatchSettlement {
                    shared: &shared,
                    dispatch_id: Some("profile-fallback-dispatch"),
                    adk_cwd: None,
                    full_response: "Recovery scheduled",
                    should_complete,
                    should_fail,
                    committed: true,
                    preserve: false,
                    resume_failure,
                    recovery_retry,
                })
                .await,
                "only the replacement turn may settle the original dispatch"
            );
        }
    }
}

#[derive(Clone)]
struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CapturingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("capturing writer lock")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturingWriter {
    type Writer = CapturingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

struct NoopGateway;

impl TurnGateway for NoopGateway {
    fn send_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        panic!("delivery epilogue test must not send a message")
    }

    fn edit_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        panic!("delivery epilogue test must not edit a message")
    }

    fn replace_message_with_outcome<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        panic!("delivery epilogue test must not replace a message")
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        _channel_id: ChannelId,
        _user_message_id: MessageId,
        _user_text: &'a str,
    ) -> GatewayFuture<'a, ()> {
        panic!("delivery epilogue test must not schedule a retry")
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        _channel_id: ChannelId,
        _intervention: &'a Intervention,
        _request_owner_name: &'a str,
        _has_more_queued_turns: bool,
        _dispatch_lease: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        panic!("delivery epilogue test must not dispatch a queued turn")
    }

    fn validate_live_routing<'a>(
        &'a self,
        _channel_id: ChannelId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn requester_mention(&self) -> Option<String> {
        None
    }

    fn can_chain_locally(&self) -> bool {
        false
    }

    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        Some(ProviderKind::Claude)
    }
}

#[tokio::test]
async fn terminal_delivery_epilogue_routes_identity_mismatch_to_warn() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let temp = tempfile::TempDir::new().expect("runtime root");
    let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let channel_id = ChannelId::new(5_025);
    let current_msg_id = MessageId::new(5_026);
    let provider = ProviderKind::Claude;
    let mut stale = InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        Some("terminal-delivery-epilogue".to_string()),
        1,
        100,
        current_msg_id.get(),
        "stale turn".to_string(),
        None,
        None,
        None,
        None,
        0,
    );
    let newer = InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        Some("terminal-delivery-epilogue".to_string()),
        1,
        200,
        current_msg_id.get(),
        "newer turn".to_string(),
        None,
        None,
        None,
        None,
        0,
    );
    crate::services::discord::inflight::save_inflight_state(&newer).expect("seed newer owner");

    let shared = crate::services::discord::make_shared_data_for_tests();
    let gateway: Arc<dyn TurnGateway> = Arc::new(NoopGateway);
    let full_response = "delivered body".to_string();
    let delivery_response = full_response.clone();
    let spoken_delivery_response = full_response.clone();
    let adk_session_key = None;
    let adk_cwd = None;
    let dispatch_id = None;
    let turn_id = "terminal-delivery-epilogue-test".to_string();
    let user_text = "user prompt".to_string();
    let mut response_sent_offset = 0;
    let mut terminal_full_replay_cleanup_msg_ids = Vec::new();
    let mut bridge_should_emit_completion = false;
    let mut status_panel_terminal_committed = false;
    let mut busy_requeue_outcome = None;

    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(CapturingWriter(buffer.clone()))
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    handle_delivery_epilogue(
        DeliveryEpilogueMessage::PostCommit,
        DeliveryEpilogueContext {
            shared_owned: &shared,
            gateway: &gateway,
            provider: &provider,
            channel_id,
            user_msg_id: None,
            current_msg_id,
            adk_session_key: &adk_session_key,
            adk_cwd: &adk_cwd,
            dispatch_id: &dispatch_id,
            turn_id: &turn_id,
            user_text_owned: &user_text,
            full_response: &full_response,
            delivery_response: &delivery_response,
            spoken_delivery_response: &spoken_delivery_response,
            cancelled: false,
            is_prompt_too_long: false,
            transport_error: false,
            recovery_retry: false,
            resume_failure_detected: false,
            claude_tui_followup_pre_submit_requeue_candidate: false,
            claude_tui_busy_requeue_pending: false,
            tui_error_classification: TuiErrorClassification::default(),
            #[cfg(unix)]
            bridge_tui_gate_outcome_early: Some(
                crate::services::discord::tmux::TuiCompletionGateOutcome::NotGated,
            ),
            terminal_delivery_committed: true,
            already_receipted: false,
            terminal_body_visible: true,
            preserve_inflight_for_cleanup_retry: false,
            should_complete_work_dispatch_after_delivery: false,
            should_fail_dispatch_after_delivery: false,
            bridge_relay_delegated_to_watcher: false,
            watcher_delivery_pin: None,
            inflight_generation: 0,
        },
        DeliveryEpilogueState {
            response_sent_offset: &mut response_sent_offset,
            inflight_state: &mut stale,
            terminal_full_replay_cleanup_msg_ids: &mut terminal_full_replay_cleanup_msg_ids,
            bridge_should_emit_completion: &mut bridge_should_emit_completion,
            status_panel_terminal_committed: &mut status_panel_terminal_committed,
            busy_requeue_outcome: &mut busy_requeue_outcome,
        },
    )
    .await;

    let logs = String::from_utf8(buffer.lock().expect("captured logs lock").clone())
        .expect("captured logs must be UTF-8");
    assert_eq!(response_sent_offset, full_response.len());
    assert!(
        logs.contains(
            "turn bridge delivered the terminal answer but could not mirror terminal_delivery_committed"
        ),
        "the production epilogue must route an identity-mismatch outcome to WARN; logs={logs}"
    );
}

// ===========================================================================
// #5191 S1-prep — a driver for `run_terminal_outcome_delivery`, plus the
// CURRENT-behaviour characterization it pins.
//
// Until this block, nothing in the tree drove `run_terminal_outcome_delivery`:
// the header on `contracts::TerminalRangeEnds` says so outright ("Nothing
// drives `run_terminal_outcome_delivery`, so no test observes which end the
// legacy fallback actually consumes"). The harness below assembles the full
// context/state pair, a fake gateway that samples `watcher.turn_delivered` at
// every publish entry, a seeded runtime root + inflight row, and a seeded
// watcher-registry slot, then drives the A5 inline terminal-replace arm.
//
// S1-prep CHANGES NO PRODUCTION BEHAVIOUR. What it fixes is the baseline: the
// marker is `false` when the bridge enters the publish, and only the epilogue's
// post-commit store turns it `true`. The S1-fix slice deliberately flips the
// first of those assertions (pre-publish CAS claim); these tests are written so
// that flip shows up as an intentional edit here rather than as silence.
// ===========================================================================

/// Publish-shaped gateway calls the driver observes, in call order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DriverCall {
    Replace,
    Edit,
    Delete,
    Send,
}

/// One observed gateway call plus the watcher marker sampled AT ENTRY.
///
/// ENTRY, not success: this is recorded when the production code reaches the
/// gateway method, before the returned future has resolved to anything. It is
/// evidence about ORDERING — whether the marker was already claimed when the
/// bridge decided to publish — and it is NOT evidence that anything was
/// published. Completion is counted separately, by
/// [`TerminalDeliveryDriver::completed_publications`], and every invariant
/// about "an answer is already out there" has to be built on THAT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DriverObservation {
    call: DriverCall,
    marker_at_entry: bool,
}

/// What the driver's terminal replace resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplaceBehaviour {
    Edited,
    /// #5191 L3: the body IS posted but the outcome is not a commit. Kept here
    /// so S1-wit can pin that known residue without re-deriving the harness.
    #[allow(dead_code)]
    FallbackAfterEditFailure,
    Failed,
    FailedPost,
    FailSecondPostOnce,
    /// Unwinds from inside the production publish call, which is how the P0
    /// rollback witness (W-P0) will reach the guard's `Drop`.
    PanicMidPublish,
}

/// Suspends its caller `remaining` times before completing. This is what gives
/// a manual-poll drop sweep real suspension points INSIDE the production call
/// graph; `wake_by_ref` keeps the same future usable from a plain `.await`.
struct Yields(usize);

impl Future for Yields {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 == 0 {
            return Poll::Ready(());
        }
        self.0 -= 1;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

struct DriverGateway {
    chain_locally: bool,
    direct: bool,
    marker: Arc<AtomicBool>,
    observations: Arc<Mutex<Vec<DriverObservation>>>,
    /// Bumped only after a publishing call has RESOLVED to a success outcome —
    /// i.e. after the suspension, at the point the production code would learn
    /// the answer is on Discord. Kept apart from the entry observations above
    /// because the two are true at different polls, and conflating them
    /// overstates by exactly one suspension what the drop sweep has witnessed.
    completed_publications: Arc<AtomicUsize>,
    published_bodies: Arc<Mutex<Vec<String>>>,
    replace: ReplaceBehaviour,
    yields_per_call: usize,
    /// When set, each send, edit or replace is checked against the watched adoption on entry.
    check: Arc<std::sync::OnceLock<crate::services::tui_o::channel_policy::BodyCheck>>,
}

impl DriverGateway {
    fn sink(&self, channel: ChannelId, op: SinkOp, content: &str) {
        if let Some(check) = self.check.get() {
            check.sink(channel.get(), op, content);
        }
    }

    fn observe(&self, call: DriverCall) {
        self.observations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(DriverObservation {
                call,
                marker_at_entry: self.marker.load(Ordering::Acquire),
            });
    }
}

impl TurnGateway for DriverGateway {
    fn send_message<'a>(
        &'a self,
        channel_id: ChannelId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        self.observe(DriverCall::Send);
        self.sink(channel_id, SinkOp::Post, _content);
        let yields = self.yields_per_call;
        let completed = Arc::clone(&self.completed_publications);
        let bodies = self.published_bodies.clone();
        let body = _content.to_owned();
        let failed = self.replace == ReplaceBehaviour::FailedPost
            || (self.replace == ReplaceBehaviour::FailSecondPostOnce
                && self
                    .observations
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|o| o.call == DriverCall::Send)
                    .count()
                    == 2);
        Box::pin(async move {
            Yields(yields).await;
            if failed {
                return Err("driver POST failed".into());
            }
            let index = completed.fetch_add(1, Ordering::Release);
            bodies.lock().unwrap().push(body);
            Ok(MessageId::new(DRIVER_FALLBACK_ANCHOR_MSG_ID + index as u64))
        })
    }

    fn edit_message<'a>(
        &'a self,
        channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        self.observe(DriverCall::Edit);
        self.sink(channel_id, SinkOp::Patch, _content);
        let yields = self.yields_per_call;
        Box::pin(async move {
            Yields(yields).await;
            Ok(())
        })
    }

    fn delete_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        self.observe(DriverCall::Delete);
        let yields = self.yields_per_call;
        Box::pin(async move {
            Yields(yields).await;
            Ok(())
        })
    }

    fn replace_message_with_outcome<'a>(
        &'a self,
        channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        self.observe(DriverCall::Replace);
        self.sink(channel_id, SinkOp::Patch, _content);
        let (yields, behaviour) = (self.yields_per_call, self.replace);
        let completed = Arc::clone(&self.completed_publications);
        let bodies = self.published_bodies.clone();
        let body = _content.to_owned();
        Box::pin(async move {
            Yields(yields).await;
            match behaviour {
                ReplaceBehaviour::Edited | ReplaceBehaviour::FailSecondPostOnce => {
                    completed.fetch_add(1, Ordering::Release);
                    bodies.lock().unwrap().push(body);
                    Ok(ReplaceLongMessageOutcome::EditedOriginal)
                }
                ReplaceBehaviour::FallbackAfterEditFailure => {
                    completed.fetch_add(1, Ordering::Release);
                    bodies.lock().unwrap().push(body);
                    Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                        edit_error: "edit 500; fallback POST succeeded".to_string(),
                        replacement_anchor: Some(MessageId::new(DRIVER_FALLBACK_ANCHOR_MSG_ID)),
                    })
                }
                ReplaceBehaviour::Failed | ReplaceBehaviour::FailedPost => {
                    Err("driver terminal replace failed".to_string())
                }
                ReplaceBehaviour::PanicMidPublish => panic!("{DRIVER_PUBLISH_PANIC}"),
            }
        })
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        _channel_id: ChannelId,
        _user_message_id: MessageId,
        _user_text: &'a str,
    ) -> GatewayFuture<'a, ()> {
        panic!("terminal delivery driver must not schedule a retry")
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        _channel_id: ChannelId,
        _intervention: &'a Intervention,
        _request_owner_name: &'a str,
        _has_more_queued_turns: bool,
        _dispatch_lease: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        panic!("terminal delivery driver must not dispatch a queued turn")
    }

    fn validate_live_routing<'a>(
        &'a self,
        _channel_id: ChannelId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn requester_mention(&self) -> Option<String> {
        None
    }

    fn can_chain_locally(&self) -> bool {
        self.chain_locally
    }

    fn can_deliver_directly(&self) -> bool {
        self.direct
    }

    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        Some(ProviderKind::Claude)
    }
}

const DRIVER_CHANNEL_ID: u64 = 5_191_001;
const DRIVER_USER_MSG_ID: u64 = 5_191_002;
const DRIVER_CURRENT_MSG_ID: u64 = 5_191_003;
const DRIVER_STALE_PREFIX_MSG_ID: u64 = 5_191_004;
const DRIVER_FALLBACK_ANCHOR_MSG_ID: u64 = 5_191_005;
const DRIVER_TMUX_SESSION: &str = "adk-5191-driver";
const DRIVER_BODY: &str = "terminal answer body for the #5191 delivery driver";
const DRIVER_PUBLISH_PANIC: &str = "driver panic inside the terminal publish";
/// Hard bound on the manual-poll sweep. Expiry is a FAILURE, never a quiet
/// green: a driver that stops making progress must look like a broken witness.
const DRIVER_POLL_BUDGET: usize = 4_096;
/// Hard wall-clock bound for the `.await`-driven runs, same reasoning.
const DRIVER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Owns everything the driven future borrows for the length of a run.
struct TerminalDeliveryDriver {
    shared: Arc<SharedData>,
    gateway: Arc<dyn TurnGateway>,
    marker: Arc<AtomicBool>,
    observations: Arc<Mutex<Vec<DriverObservation>>>,
    completed_publications: Arc<AtomicUsize>,
    published_bodies: Arc<Mutex<Vec<String>>>,
    body_check: Arc<std::sync::OnceLock<crate::services::tui_o::channel_policy::BodyCheck>>,
    inflight: InflightTurnState,
    body: String,
    _temp: tempfile::TempDir,
    _env_reset: crate::config::TestEnvVarGuard,
    _env_lock: std::sync::MutexGuard<'static, ()>,
}

impl TerminalDeliveryDriver {
    /// Seeds a runtime root, an inflight row, and a watcher-registry slot whose
    /// `turn_delivered` marker is the coordinate under test. `yields_per_call`
    /// controls how many times each production gateway call suspends, which is
    /// what makes the drop sweep below land at real interior points rather than
    /// only before the first poll.
    fn new(replace: ReplaceBehaviour, yields_per_call: usize) -> Self {
        let env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("driver runtime root");
        let env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        let provider = ProviderKind::Claude;
        let mut inflight = InflightTurnState::new(
            provider.clone(),
            DRIVER_CHANNEL_ID,
            Some("terminal-delivery-driver".to_string()),
            1,
            DRIVER_USER_MSG_ID,
            DRIVER_CURRENT_MSG_ID,
            "driver prompt".to_string(),
            None,
            Some(DRIVER_TMUX_SESSION.to_string()),
            None,
            None,
            0,
        );
        inflight.full_response = String::new();
        crate::services::discord::inflight::save_inflight_state(&inflight)
            .expect("seed the driver's own inflight row");

        let shared = crate::services::discord::make_shared_data_for_tests();
        let marker = Arc::new(AtomicBool::new(false));
        shared.tmux_watchers.insert(
            ChannelId::new(DRIVER_CHANNEL_ID),
            TmuxWatcherHandle {
                tmux_session_name: DRIVER_TMUX_SESSION.to_string(),
                output_path: temp.path().join("driver.jsonl").display().to_string(),
                paused: Arc::new(AtomicBool::new(false)),
                resume_offset: Arc::new(Mutex::new(None)),
                cancel: Arc::new(AtomicBool::new(false)),
                pause_epoch: Arc::new(AtomicU64::new(0)),
                turn_delivered: Arc::clone(&marker),
                last_heartbeat_ts_ms: Arc::new(AtomicI64::new(
                    crate::services::discord::tmux_watcher_registry::tmux_watcher_now_ms(),
                )),
            },
        );

        let observations = Arc::new(Mutex::new(Vec::new()));
        let completed_publications = Arc::new(AtomicUsize::new(0));
        let published_bodies = Arc::new(Mutex::new(Vec::new()));
        let body_check = Arc::new(std::sync::OnceLock::new());
        let gateway: Arc<dyn TurnGateway> = Arc::new(DriverGateway {
            chain_locally: true,
            direct: true,
            marker: Arc::clone(&marker),
            observations: Arc::clone(&observations),
            completed_publications: Arc::clone(&completed_publications),
            published_bodies: published_bodies.clone(),
            replace,
            yields_per_call,
            check: body_check.clone(),
        });

        Self {
            shared,
            gateway,
            marker,
            observations,
            completed_publications,
            published_bodies,
            body_check,
            inflight,
            body: DRIVER_BODY.to_string(),
            _temp: temp,
            _env_reset: env_reset,
            _env_lock: env_lock,
        }
    }

    /// Swap the answer body. A body that needs several Discord messages routes
    /// the same driver down the legacy long-chunk arm instead of the inline
    /// replace.
    fn with_body(mut self, body: String) -> Self {
        self.body = body;
        self
    }

    fn marker(&self) -> bool {
        self.marker.load(Ordering::Acquire)
    }

    /// How many publishing calls have RESOLVED successfully. This — not the
    /// entry observations — is what "an answer is already on Discord" means.
    fn completed_publications(&self) -> usize {
        self.completed_publications.load(Ordering::Acquire)
    }

    fn observations(&self) -> Vec<DriverObservation> {
        self.observations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Entry observations for the inline replace. ORDERING evidence only — see
    /// [`DriverObservation`]. A non-empty result does not mean anything was
    /// published.
    fn publish_entries(&self) -> Vec<DriverObservation> {
        self.observations()
            .into_iter()
            .filter(|observed| observed.call == DriverCall::Replace)
            .collect()
    }

    /// The A5 inline terminal-replace arm: no watcher/standby output owner, a
    /// non-empty body, `can_chain_locally`, no admitted Codex frame (so the
    /// pinned macro no-ops), and `tmux_last_offset = None` so the short-replace
    /// cut-over decision stays false and the legacy inline replace runs.
    fn parts(&self) -> (TerminalOutcomeDeliveryContext, TerminalOutcomeDeliveryState) {
        let channel_id = ChannelId::new(DRIVER_CHANNEL_ID);
        (
            TerminalOutcomeDeliveryContext {
                preloop_receipt_confirmed: false,
                entry_was_rowless: false,
                watcher_delivery_pin: self
                    .shared
                    .tmux_watchers
                    .get(&channel_id)
                    .map(|h| WatcherClaimIncarnation::from_handle(channel_id, &h)),
                channel_id,
                user_msg_id: Some(MessageId::new(DRIVER_USER_MSG_ID)),
                current_msg_id: MessageId::new(DRIVER_CURRENT_MSG_ID),
                status_panel_msg_id: None,
                cancelled: false,
                transport_error: false,
                recovery_retry: false,
                rx_disconnected: false,
                tmux_last_offset: None,
                codex_tui_terminal_range: None,
                watcher_owner_channel_id: channel_id,
                watcher_handoff_claim_outcome: WatcherHandoffClaimOutcome::None,
                bridge_created_response_placeholder_msg_id: None,
                bridge_relay_delegated_to_watcher: false,
                bridge_output_owner: None,
                should_complete_work_dispatch_after_delivery: false,
                should_fail_dispatch_after_delivery: false,
                single_message_panel_footer_mode: false,
                is_prompt_too_long: false,
                claude_tui_followup_pre_submit_requeue_candidate: false,
                tui_error_classification: TuiErrorClassification::default(),
                had_prior_session_id_at_turn_start: false,
                session_handshake_seen: true,
                turn_start: std::time::Instant::now(),
                #[cfg(unix)]
                bridge_tui_gate_outcome_early: Some(
                    crate::services::discord::tmux::TuiCompletionGateOutcome::NotGated,
                ),
            },
            TerminalOutcomeDeliveryState {
                shared_owned: Arc::clone(&self.shared),
                gateway: Arc::clone(&self.gateway),
                provider: ProviderKind::Claude,
                cancel_token: Arc::new(crate::services::provider::CancelToken::new()),
                turn_id: "terminal-delivery-driver-5191".to_string(),
                user_text_owned: "driver prompt".to_string(),
                adk_session_key: None,
                adk_cwd: None,
                dispatch_id: None,
                new_session_id: None,
                new_raw_provider_session_id: None,
                full_response: self.body.clone(),
                active_background_child_session_ids: Vec::new(),
                pending_long_running_open_after_state_save: None,
                pending_long_running_retarget_after_state_save: None,
                long_running_placeholder_active: None,
                inflight_state: self.inflight.clone(),
                api_friction_reports: Vec::new(),
                review_dispatch_warning: None,
                last_edit_text: String::new(),
                terminal_empty_response_notice: None,
                // Drives the epilogue's post-commit prefix drain, which is the
                // one production suspension point INSIDE the epilogue the drop
                // sweep can land on.
                terminal_full_replay_cleanup_msg_ids: vec![MessageId::new(
                    DRIVER_STALE_PREFIX_MSG_ID,
                )],
                resume_failure_detected: false,
                response_sent_offset: 0,
            },
        )
    }
}

/// Polls `future` at most `polls` times and reports whether it completed. The
/// budget is a hard bound: running out is reported to the caller as "did not
/// complete", and every caller turns that into a FAILURE rather than a pass.
fn poll_at_most<F: Future>(future: &mut Pin<Box<F>>, polls: usize) -> bool {
    let mut cx = Context::from_waker(std::task::Waker::noop());
    for _ in 0..polls {
        if future.as_mut().poll(&mut cx).is_ready() {
            return true;
        }
    }
    false
}

/// #5191 S1-prep baseline. The bridge enters its terminal publish with
/// `watcher.turn_delivered` STILL FALSE, and only the epilogue's post-commit
/// store turns it true afterwards.
///
/// This is the exact ordering the duplicate-relay symptom rides on: between the
/// publish landing on Discord and the epilogue store, a resuming watcher reads
/// `false` and relays the same answer again. S1-fix moves a CAS claim ahead of
/// the fork, at which point `marker_at_entry` below becomes `true` — an
/// intentional edit to this assertion, not a silent behaviour change.
#[tokio::test]
async fn driver_terminal_publish_currently_starts_with_the_watcher_marker_unset_5191() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    assert!(
        !driver.marker(),
        "the driver starts from an unclaimed marker"
    );

    let (ctx, state) = driver.parts();
    let output = tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
        .await
        .expect("terminal outcome delivery must not hang");

    let publishes = driver.publish_entries();
    assert_eq!(
        publishes.len(),
        1,
        "the driver must reach the inline terminal replace exactly once; observed={:?}",
        driver.observations()
    );
    assert!(
        !publishes[0].marker_at_entry,
        "BASELINE: today the bridge publishes with turn_delivered still false"
    );
    assert!(
        driver.marker(),
        "the epilogue's post-commit store is what leaves the marker true today"
    );
    assert!(
        output.terminal_delivery_committed,
        "an EditedOriginal replace commits the terminal delivery"
    );
    assert!(
        !output.preserve_inflight_for_cleanup_retry,
        "a committed delivery does not preserve inflight for retry"
    );
}

/// #5191 S1-prep baseline. A replace that is NOT committed preserves the turn
/// for retry, and `bridge_epilogue_marks_watcher_delivered` therefore refuses to
/// mark the watcher — the marker stays false end to end.
///
/// S1-fix must keep this false: a claim taken before the fork has to be ROLLED
/// BACK here. If it ever reports true, the watcher is permanently suppressed for
/// a turn that was never delivered, which is the slice's absolute-line failure.
#[tokio::test]
async fn driver_uncommitted_terminal_replace_leaves_the_watcher_unmarked_5191() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Failed, 1);

    let (ctx, state) = driver.parts();
    let output = tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
        .await
        .expect("terminal outcome delivery must not hang");

    assert_eq!(driver.publish_entries().len(), 1);
    assert!(
        output.preserve_inflight_for_cleanup_retry,
        "a failed replace preserves the turn for retry"
    );
    assert!(
        !output.terminal_delivery_committed,
        "a failed replace does not commit the terminal delivery"
    );
    assert!(
        !driver.marker(),
        "an undelivered turn must never suppress the watcher"
    );
}

/// #5191 S1-prep: the driver can drive an UNWIND out of the production publish
/// and observe the marker afterwards. That capability is the whole reason this
/// harness exists ahead of S1-fix — the P0 witness (a claim leaking through a
/// panic and permanently suppressing the watcher) is unreachable without it.
///
/// The baseline value is `false` because nothing claims the marker yet. After
/// S1-fix the claim sets it true before the publish and the guard's `Drop` must
/// restore it to false along this same unwind; the assertion text stays, the
/// mechanism under it changes.
#[tokio::test]
async fn driver_publish_panic_unwinds_and_leaves_the_watcher_unmarked_5191() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::PanicMidPublish, 1);
    let (ctx, state) = driver.parts();
    let mut future = Box::pin(run_terminal_outcome_delivery(ctx, state));

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        poll_at_most(&mut future, DRIVER_POLL_BUDGET)
    }));

    let payload = unwound.expect_err("the production publish must unwind the driven future");
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or_default()
        .to_string();
    assert!(
        message.contains(DRIVER_PUBLISH_PANIC),
        "the unwind must come from inside the publish, not from the harness; payload={message}"
    );
    assert_eq!(
        driver.publish_entries().len(),
        1,
        "the unwind must happen at the publish seam"
    );
    assert_eq!(
        driver.completed_publications(),
        0,
        "the panicking publish never resolved, so nothing landed"
    );
    assert!(
        !driver.marker(),
        "an unwound turn must never leave the watcher suppressed"
    );
    drop(future);
}

/// #5191 S1-prep: the manual-poll DROP SWEEP, and the marker table it measures.
///
/// Every gateway call the driver serves suspends once, so dropping the driven
/// future after `n` polls lands at real interior points of the production call
/// graph. Today the table is uniform: no reachable drop point leaves the marker
/// set, because only the epilogue's post-commit store sets it and nothing
/// suspends after that store.
///
/// S1-fix inverts the interesting half — once a publish has RESOLVED
/// successfully, a drop must leave the marker `true` — and that inversion is
/// what kills a mutant that moves `settle` behind the epilogue.
///
/// ENTRY IS NOT SUCCESS. The fake gateway records an entry observation when the
/// production code reaches the method, then suspends, and only resolves the
/// outcome on a later poll. So a drop at the poll that entered the publish is
/// NOT a post-success drop: nothing was delivered there, and rolling the marker
/// back at that point is correct rather than a defect. The invariant and the
/// counting below are therefore built on `completed_publications`, never on the
/// entry observations. Measured for this fixture (one suspension per gateway
/// call): the publish resolves on poll 2, and the single post-success
/// non-completing drop point is poll 2 — the epilogue's stale-prefix drain.
///
/// MEASURED CONSTRAINT (#5191 U7). A spin-polled task cannot resolve the
/// epilogue's voice-completion lookup: `voice_channel_for_background` awaits
/// `cached_config`, which awaits a `tokio::task::spawn_blocking` config load
/// that never resolves while the polling task itself monopolises the
/// current-thread runtime. The sweep therefore drives the production shape that
/// skips it — a turn with no anchored user message, which the epilogue already
/// documents as a real recovery shape ("A recovery turn with no anchored user
/// message (user_msg_id == 0) is never a voice turn"). This is an ordinary
/// production input, not a test-only bypass: the publish, the stale-prefix
/// drain and the post-commit marker store all still run. The `.await`-driven
/// tests above cover the anchored-user-message shape.
#[tokio::test]
async fn driver_drop_sweep_measures_the_current_marker_at_every_suspension_5191() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    // (polls, completed, publications_landed, marker)
    let mut table: Vec<(usize, bool, usize, bool)> = Vec::new();
    let mut polls_to_complete = None;
    for polls in 0..DRIVER_POLL_BUDGET {
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
        let (mut ctx, state) = driver.parts();
        ctx.user_msg_id = None;
        let mut future = Box::pin(run_terminal_outcome_delivery(ctx, state));
        let completed = poll_at_most(&mut future, polls);
        drop(future);
        table.push((
            polls,
            completed,
            driver.completed_publications(),
            driver.marker(),
        ));
        if completed {
            polls_to_complete = Some(polls);
            break;
        }
    }
    let polls_to_complete =
        polls_to_complete.expect("the driven future must complete inside the sweep's poll budget");

    for (polls, completed, landed, marker) in &table {
        if *completed {
            assert!(
                *marker,
                "a completed committed delivery must leave the watcher marked (polls={polls})"
            );
        } else {
            assert!(
                !*marker,
                "BASELINE: dropping at poll {polls} leaves the watcher unmarked \
                 (publications_landed={landed})"
            );
        }
    }

    // The sweep is only a witness for a late settle if it can drop AFTER a
    // publish actually landed. Measured value for this fixture: exactly one
    // such point. S1-fix inverts the marker expectation there.
    let post_success_drop_points = table
        .iter()
        .filter(|(_, completed, landed, _)| !*completed && *landed > 0)
        .count();
    assert_eq!(
        post_success_drop_points, 1,
        "measured shape: exactly one drop point sits after a publish resolved and \
         before the future completes; table={table:?}"
    );
    assert!(
        polls_to_complete >= 3,
        "the sweep needs interior suspension points to be a witness at all, \
         but the future completed in {polls_to_complete} polls"
    );
}

/// #5191 S1-prep (U8 pre-measurement): the legacy long-chunk arm is reachable
/// from this driver. A body that needs several Discord messages, with no
/// ordered tmux range, routes past both cut-over decisions into
/// `apply_bridge_long_chunks_legacy`, which sends new chunks and deletes the
/// placeholder instead of replacing in place.
///
/// That matters because the ordering witnesses have to cover more than one
/// publishing arm: a claim placed correctly for the inline replace says nothing
/// about this one. The baseline is the same — this arm also publishes with the
/// watcher marker still unset.
#[tokio::test]
async fn driver_reaches_the_legacy_long_chunk_arm_with_an_unordered_range_5191() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let driver =
        TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1).with_body("chunk ".repeat(1_200));
    let (ctx, state) = driver.parts();
    let output = tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
        .await
        .expect("terminal outcome delivery must not hang");

    let observed = driver.observations();
    assert!(
        observed.iter().any(|call| call.call == DriverCall::Send),
        "the long-chunk arm sends new chunks; observed={observed:?}"
    );
    assert!(
        driver.publish_entries().is_empty(),
        "the long-chunk arm must not take the inline replace; observed={observed:?}"
    );
    assert!(
        observed.iter().all(|call| !call.marker_at_entry),
        "BASELINE: the long-chunk arm also publishes with turn_delivered unset"
    );
    assert!(
        output.terminal_delivery_committed,
        "a successful long-chunk send commits the terminal delivery"
    );
}

#[tokio::test]
async fn resume_pin_delivery_epilogue_stamps_only_current_incarnation() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for case in ["same", "stale", "missing", "cancelled"] {
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
        let (mut ctx, state) = driver.parts();
        let owner = ctx.watcher_owner_channel_id;
        let mut replacement_marker = None;
        if case == "stale" {
            let old = driver.shared.tmux_watchers.get(&owner).unwrap();
            let marker = Arc::new(AtomicBool::new(false));
            let replacement = TmuxWatcherHandle {
                tmux_session_name: old.tmux_session_name.clone(),
                output_path: old.output_path.clone(),
                paused: Arc::new(AtomicBool::new(false)),
                resume_offset: Arc::new(Mutex::new(None)),
                cancel: Arc::new(AtomicBool::new(false)),
                pause_epoch: Arc::new(AtomicU64::new(0)),
                turn_delivered: marker.clone(),
                last_heartbeat_ts_ms: Arc::new(AtomicI64::new(0)),
            };
            drop(old);
            driver.shared.tmux_watchers.insert(owner, replacement);
            replacement_marker = Some(marker);
        } else if case == "missing" {
            ctx.watcher_delivery_pin = None;
        } else if case == "cancelled" {
            driver
                .shared
                .tmux_watchers
                .get(&owner)
                .unwrap()
                .cancel
                .store(true, Ordering::Release);
        }
        let output =
            tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
                .await
                .expect("delivery epilogue must complete");
        assert!(
            output.terminal_delivery_committed,
            "{case}: delivery must commit"
        );
        assert_eq!(
            driver.publish_entries().len(),
            1,
            "{case}: real publish required"
        );
        if let Some(marker) = replacement_marker {
            assert!(
                !marker.load(Ordering::Acquire),
                "stale epilogue must not stamp replacement B"
            );
        }
        assert_eq!(driver.marker(), case == "same", "{case}: captured A marker");
    }
}

mod rest_delivery_tests;

/// One real bridge stream tick over `inflight`'s body; returns the tick's offset and row.
async fn drive_bridge_stream_tick(
    driver: &TerminalDeliveryDriver,
    mut inflight_state: InflightTurnState,
) -> (usize, String, InflightTurnState) {
    use crate::services::discord::turn_bridge::stream_tick::{
        BridgeStreamTickContext, BridgeStreamTickState, run_bridge_stream_tick,
    };
    let expected =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(&inflight_state);
    let mut baseline = inflight_state.clone();
    let mut expected_current_message = (
        inflight_state.current_msg_id,
        inflight_state.current_msg_len,
    );
    let mut current_msg_id = crate::services::discord::turn_bridge::current_message_anchor::detached_current_msg_id_from_durable(
        inflight_state.current_msg_id,
    );
    let mut full_response = driver.body.clone();
    let (mut response_sent_offset, mut confirmed_offset) = (0usize, 0usize);
    let (mut state_dirty, mut status_panel_dirty, mut first_answer_relayed) = (false, false, true);
    let (mut watcher_owns, mut watcher_available, mut standby_owns) = (false, false, false);
    let (mut any_tool_used, mut has_post_tool_text) = (false, false);
    let now = tokio::time::Instant::now;
    let (mut lifecycle_refresh, mut panel_edit, mut status_edit) = (now(), now(), now());
    let (mut spin_idx, mut status_panel_generation) = (0usize, 0u64);
    let mut status_panel_msg_id: Option<MessageId> = None;
    let mut last_status_panel_text = String::new();
    let mut watcher_delivery_pin = None;
    let mut watcher_owner_channel_id = ChannelId::new(DRIVER_CHANNEL_ID);
    let mut frozen: Vec<MessageId> = Vec::new();
    let (mut pending_candidate, mut created_placeholder): (Option<MessageId>, Option<MessageId>) =
        (None, None);
    let mut last_edit_text = String::new();
    let (mut current_tool_line, mut prev_tool_status) = (None, None);
    let (mut last_tool_name, mut last_tool_summary) = (None, None);
    let mut tmux_last_offset: Option<u64> = None;
    let mut bridge_spans = crate::services::discord::turn_bridge::bridge_latency_spans::BridgeLatencySpans::starting_at(
        std::time::Instant::now(),
    );
    let (mut open_after_save, mut retarget_after_save, mut long_running_active) =
        (None, None, None);
    let (mut adk_heartbeat, mut long_run_heartbeat) =
        (std::time::Instant::now(), std::time::Instant::now());
    let outcome = run_bridge_stream_tick(
        BridgeStreamTickContext {
            shared_owned: Arc::clone(&driver.shared),
            gateway: Arc::clone(&driver.gateway),
            channel_id: ChannelId::new(DRIVER_CHANNEL_ID),
            provider: &ProviderKind::Claude,
            turn_id: "terminal-delivery-driver-5191",
            expected_identity: &expected,
            status_interval: std::time::Duration::from_secs(3_600),
            single_message_panel_footer_mode: false,
            footer_owner:
                crate::services::discord::footer_view_reconciler::CompletionFooterOwner::new(
                    DRIVER_USER_MSG_ID,
                    0,
                ),
            status_panel_started_at: 0,
            done: false,
            dispatch_id: None,
            adk_session_key: None,
            adk_session_name: None,
            adk_session_info: None,
            adk_cwd: None,
            role_binding: None,
            spinner: &["|"],
            live_long_run_heartbeat_interval: std::time::Duration::from_secs(3_600),
        },
        BridgeStreamTickState {
            state_dirty: &mut state_dirty,
            last_session_panel_lifecycle_refresh: &mut lifecycle_refresh,
            status_panel_dirty: &mut status_panel_dirty,
            spin_idx: &mut spin_idx,
            last_status_panel_edit: &mut panel_edit,
            last_status_edit: &mut status_edit,
            status_panel_msg_id: &mut status_panel_msg_id,
            last_status_panel_text: &mut last_status_panel_text,
            watcher_owns_assistant_relay: &mut watcher_owns,
            watcher_relay_available_for_turn: &mut watcher_available,
            watcher_delivery_pin: &mut watcher_delivery_pin,
            standby_relay_owns_output: &mut standby_owns,
            watcher_owner_channel_id: &mut watcher_owner_channel_id,
            full_response: &mut full_response,
            response_sent_offset: &mut response_sent_offset,
            bridge_confirmed_response_sent_offset: &mut confirmed_offset,
            streaming_rollover_frozen_msg_ids: &mut frozen,
            current_msg_id: &mut current_msg_id,
            expected_current_message: &mut expected_current_message,
            pending_current_message_candidate: &mut pending_candidate,
            bridge_created_response_placeholder_msg_id: &mut created_placeholder,
            last_edit_text: &mut last_edit_text,
            first_answer_relayed: &mut first_answer_relayed,
            current_tool_line: &mut current_tool_line,
            prev_tool_status: &mut prev_tool_status,
            last_tool_name: &mut last_tool_name,
            last_tool_summary: &mut last_tool_summary,
            any_tool_used: &mut any_tool_used,
            has_post_tool_text: &mut has_post_tool_text,
            tmux_last_offset: &mut tmux_last_offset,
            persisted_inflight_baseline: &mut baseline,
            inflight_state: &mut inflight_state,
            bridge_spans: &mut bridge_spans,
            status_panel_generation: &mut status_panel_generation,
            pending_long_running_open_after_state_save: &mut open_after_save,
            pending_long_running_retarget_after_state_save: &mut retarget_after_save,
            long_running_placeholder_active: &mut long_running_active,
            last_adk_heartbeat: &mut adk_heartbeat,
            last_inflight_long_run_heartbeat: &mut long_run_heartbeat,
        },
    )
    .await;
    assert_eq!(
        outcome,
        crate::services::discord::turn_bridge::stream_tick::StreamTickOutcome::Continue,
        "the tick must run to its body section"
    );
    (response_sent_offset, full_response, inflight_state)
}

/// A delegated TUI turn on a direct gateway leaves the body to O through tick and terminal,
/// while a headless gateway turn of the same kind keeps its whole body for Legacy delivery.
#[tokio::test]
async fn o_delegated_tui_body_is_cut_on_direct_gateways_but_not_headless() {
    for direct in [true, false] {
        let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
        driver.gateway = Arc::new(DriverGateway {
            chain_locally: true,
            direct,
            marker: driver.marker.clone(),
            observations: driver.observations.clone(),
            completed_publications: driver.completed_publications.clone(),
            published_bodies: driver.published_bodies.clone(),
            replace: ReplaceBehaviour::Edited,
            yields_per_call: 0,
            check: driver.body_check.clone(),
        });
        driver.inflight.runtime_kind =
            Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        crate::services::discord::inflight::save_inflight_state(&driver.inflight)
            .expect("seed the TUI-kind row");
        let _forced = crate::services::tui_o::cutover::test_override::force_channels(&[(
            DRIVER_CHANNEL_ID,
            crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        )]);

        let (offset, full_response, inflight) =
            drive_bridge_stream_tick(&driver, driver.inflight.clone()).await;
        // The last chunk lands after the final tick, so the terminal sees an unsent tail.
        const TAIL: &str = "\nADK tail streamed after the last tick";
        let (ctx, mut state) = driver.parts();
        (
            state.response_sent_offset,
            state.full_response,
            state.inflight_state,
        ) = (offset, format!("{full_response}{TAIL}"), inflight);
        let output =
            tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
                .await
                .expect("terminal outcome delivery must not hang");

        let observed = driver.observations();
        let body_writes = observed
            .iter()
            .filter(|o| {
                matches!(
                    o.call,
                    DriverCall::Replace | DriverCall::Send | DriverCall::Edit
                )
            })
            .count();
        assert_eq!(
            body_writes, 0,
            "direct={direct}: no gateway body write; observed={observed:?}"
        );
        if direct {
            assert_eq!(
                offset,
                DRIVER_BODY.len(),
                "the tick consumes the delegated body"
            );
            assert!(
                output.terminal_delivery_committed,
                "consumed turn commits without transport"
            );
        } else {
            assert_eq!(offset, 0, "a headless turn keeps its body unconsumed");
            assert_eq!(
                (output.full_response, output.response_sent_offset),
                (format!("{DRIVER_BODY}{TAIL}"), 0),
                "a held headless gateway retains the whole original body"
            );
            assert!(
                !observed.iter().any(|o| o.call == DriverCall::Delete),
                "headless placeholder is not dropped by the O cut; observed={observed:?}"
            );
            assert!(
                !output.terminal_delivery_committed && output.preserve_inflight_for_cleanup_retry,
                "a selected TUI destination without a direct gateway is held for retry"
            );
        }
    }
}

// Cancellation uses the destination membership and holds uncertain selected identities without consuming them.
#[tokio::test]
async fn o_channel_cancel_uses_destination_and_holds_unknown_kind() {
    use crate::services::agent_protocol::RuntimeHandoffKind::{ClaudeTui, CodexTui};

    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(CapturingWriter(buffer.clone()))
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let a = DRIVER_CHANNEL_ID;
    let b = DRIVER_CHANNEL_ID + 100;
    let cases = [
        (
            "selected_destination",
            a,
            b,
            true,
            true,
            Some(ClaudeTui),
            0,
            false,
        ),
        (
            "selected_owner_only",
            b,
            a,
            true,
            true,
            Some(ClaudeTui),
            1,
            false,
        ),
        (
            "empty_membership",
            a,
            b,
            false,
            true,
            Some(ClaudeTui),
            1,
            false,
        ),
        ("selected_unknown_kind", a, b, true, true, None, 0, true),
        ("outside_unknown_kind", b, a, true, true, None, 1, false),
        (
            "selected_changed_kind",
            a,
            b,
            true,
            true,
            Some(CodexTui),
            0,
            true,
        ),
        (
            "flag_off_selected",
            a,
            b,
            true,
            false,
            Some(ClaudeTui),
            1,
            false,
        ),
    ];
    for (name, destination, owner, selected, enabled, kind, writes, held) in cases {
        buffer.lock().unwrap().clear();
        let body = format!("writer cancellation body for {name}");
        let driver =
            TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0).with_body(body.clone());
        let (mut ctx, mut state) = driver.parts();
        ctx.cancelled = true;
        ctx.channel_id = ChannelId::new(destination);
        ctx.watcher_owner_channel_id = ChannelId::new(owner);
        ctx.watcher_delivery_pin = None;
        ctx.user_msg_id = None;
        state.inflight_state.channel_id = destination;
        state.inflight_state.runtime_kind = kind;
        state.terminal_full_replay_cleanup_msg_ids.clear();
        crate::services::discord::inflight::save_inflight_state(&state.inflight_state)
            .expect("seed matching destination row");
        let channels = if selected {
            vec![(a, ClaudeTui)]
        } else {
            Vec::new()
        };
        let _o = crate::services::tui_o::cutover::test_override::force_channels(&channels);
        let _disabled = (!enabled).then(crate::services::tui_o::cutover::test_override::force_off);

        let output =
            tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
                .await
                .expect("cancel delivery must finish");
        let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        assert_eq!(
            logs.contains("tui_o output identity held"),
            held,
            "{name}: {logs}"
        );
        let observed = driver.observations();
        let body_calls = observed
            .iter()
            .filter(|observation| {
                matches!(
                    observation.call,
                    DriverCall::Replace | DriverCall::Send | DriverCall::Edit
                )
            })
            .count();
        assert_eq!(body_calls, writes, "{name}: {observed:?}");
        assert_eq!(driver.completed_publications(), writes, "{name}");
        let published = driver.published_bodies.lock().unwrap();
        assert_eq!(published.len(), writes, "{name}: {published:?}");
        if writes == 1 {
            assert!(published[0].contains(&body), "{name}: body preserved");
            assert!(
                published[0].contains("[Stopped]"),
                "{name}: cancel lifecycle preserved"
            );
        }
        if held {
            assert!(
                output.preserve_inflight_for_cleanup_retry,
                "{name}: hold preserves retry"
            );
            assert!(
                !output.status_panel_terminal_committed,
                "{name}: hold is not successful consumption"
            );
            assert!(
                !output.terminal_delivery_committed,
                "{name}: hold is not delivery evidence"
            );
            assert_eq!(
                output.response_sent_offset, 0,
                "{name}: hold retains body offset"
            );
            assert_eq!(output.full_response, body, "{name}: hold retains body");
            assert!(
                !observed
                    .iter()
                    .any(|observation| observation.call == DriverCall::Delete),
                "{name}: hold retains the existing placeholder"
            );
        }
    }
}

/// A /stop on a listed channel's TUI turn drops only the Legacy placeholder: the cancelled
/// partial body is O's, so no replace carries it, while an unlisted channel still shows it.
#[tokio::test]
async fn o_delegated_cancelled_partial_body_is_not_replaced() {
    for delegated in [false, true] {
        let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
        driver.inflight.runtime_kind =
            Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        crate::services::discord::inflight::save_inflight_state(&driver.inflight)
            .expect("seed the TUI-kind row");
        let listed = if delegated {
            vec![(
                driver.inflight.channel_id,
                crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            )]
        } else {
            Vec::new()
        };
        let _o = crate::services::tui_o::cutover::test_override::force_channels(&listed);
        let (mut ctx, state) = driver.parts();
        ctx.cancelled = true;
        let output =
            tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
                .await
                .expect("terminal outcome delivery must not hang");
        let observed = driver.observations();
        let shown = driver.published_bodies.lock().unwrap().clone();
        let body_shown = shown.iter().any(|body| body.contains(DRIVER_BODY));
        assert_eq!(body_shown, !delegated, "delegated={delegated}: {shown:?}");
        if delegated {
            let writes = observed.iter().filter(|o| {
                matches!(
                    o.call,
                    DriverCall::Replace | DriverCall::Send | DriverCall::Edit
                )
            });
            assert_eq!(writes.count(), 0, "observed={observed:?}");
            assert!(
                observed.iter().any(|o| o.call == DriverCall::Delete),
                "the Legacy placeholder is dropped; observed={observed:?}"
            );
            assert!(!output.preserve_inflight_for_cleanup_retry);
        }
    }
}

/// A terminal or /stop with no answer to publish (empty, whitespace or TUI chrome only) leaves a
/// pending adoption; one that publishes the body ends it first, and Legacy shows that body once.
#[tokio::test]
async fn only_a_terminal_or_stop_with_text_ends_a_pending_adoption() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::channel_policy::{Adoption, BodyCheck};
    use crate::services::tui_o::cutover::test_override;
    for (cancelled, body) in [
        (false, ""),
        (false, " \n"),
        (false, "No response requested."),
        (true, ""),
        (true, " \n"),
        (false, DRIVER_BODY),
        (true, DRIVER_BODY),
    ] {
        let mut driver =
            TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0).with_body(body.to_string());
        driver.inflight.runtime_kind = Some(ClaudeTui);
        crate::services::discord::inflight::save_inflight_state(&driver.inflight)
            .expect("seed the TUI-kind row");
        let _candidates = test_override::force_candidates(&[(DRIVER_CHANNEL_ID, ClaudeTui)]);
        let check = BodyCheck::watch(DRIVER_CHANNEL_ID, DRIVER_BODY);
        driver.body_check.set(check.clone()).unwrap();
        let (mut ctx, state) = driver.parts();
        ctx.cancelled = cancelled;
        tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
            .await
            .expect("terminal outcome delivery must not hang");
        let shown = driver.published_bodies.lock().unwrap().clone();
        let with_body = body == DRIVER_BODY;
        let case = format!("cancelled={cancelled} body={body:?}: {shown:?}");
        check.assert_settled();
        let expected = if with_body {
            Adoption::Released
        } else {
            Adoption::Pending
        };
        assert_eq!(check.adoption(), expected, "{case}");
        let bodies = shown
            .iter()
            .filter(|shown| shown.contains(DRIVER_BODY))
            .count();
        assert_eq!(bodies, usize::from(with_body), "{case}");
    }
}

/// A terminal whose delivery lease another holder keeps sends no body, so a pending adoption
/// stays pending.
#[tokio::test]
async fn a_terminal_that_loses_its_delivery_lease_leaves_a_pending_adoption() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::channel_policy::{Adoption, BodyCheck};
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
    driver.inflight.runtime_kind = Some(ClaudeTui);
    driver.inflight.turn_start_offset = Some(0);
    crate::services::discord::inflight::save_inflight_state(&driver.inflight)
        .expect("seed the TUI-kind row");
    let _candidates = crate::services::tui_o::cutover::test_override::force_candidates(&[(
        DRIVER_CHANNEL_ID,
        ClaudeTui,
    )]);
    let check = BodyCheck::watch(DRIVER_CHANNEL_ID, DRIVER_BODY);
    driver.body_check.set(check.clone()).unwrap();
    let channel = ChannelId::new(DRIVER_CHANNEL_ID);
    let generation = driver.shared.restart.current_generation;
    let key = bridge_delivery_lease_key_for_inflight(channel, generation, &driver.inflight);
    let holder = crate::services::discord::LeaseHolder::Watcher { instance_id: 7 };
    let deadline = crate::services::discord::lease_now_ms() + 60_000;
    let cell = driver.shared.delivery_lease(channel);
    assert!(
        cell.try_acquire(key, holder, 0, 64, deadline),
        "another holder takes the lease"
    );
    let (mut ctx, state) = driver.parts();
    ctx.tmux_last_offset = Some(64);
    tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
        .await
        .expect("terminal outcome delivery must not hang");
    let shown = driver.published_bodies.lock().unwrap().clone();
    assert!(
        !shown.iter().any(|body| body.contains(DRIVER_BODY)),
        "{shown:?}"
    );
    check.assert_settled();
    assert_eq!(check.adoption(), Adoption::Pending);
}

/// A turn the watcher owns retires the placeholder at its end only on O's channel, where the
/// placeholder is the live panel and never holds a body; a Legacy channel keeps main's choice.
#[tokio::test]
async fn a_watcher_owned_turn_end_deletes_the_o_panel_only_on_o_channels() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::cutover::test_override;
    for o_owned in [false, true] {
        let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
        driver.inflight.runtime_kind = Some(ClaudeTui);
        crate::services::discord::inflight::save_inflight_state(&driver.inflight)
            .expect("seed the TUI-kind row");
        let owned = [(DRIVER_CHANNEL_ID, ClaudeTui)];
        let _boot = test_override::force_channels(if o_owned { &owned } else { &[] });
        let (mut ctx, state) = driver.parts();
        ctx.bridge_output_owner = Some(BridgeOutputOwner::WatcherRelay);
        tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
            .await
            .expect("terminal outcome delivery must not hang");
        let calls: Vec<_> = driver.observations().into_iter().map(|o| o.call).collect();
        let deletes = calls
            .iter()
            .filter(|call| **call == DriverCall::Delete)
            .count();
        assert_eq!(
            deletes,
            usize::from(o_owned),
            "o_owned={o_owned}: {calls:?}"
        );
        assert!(
            !calls.contains(&DriverCall::Send),
            "o_owned={o_owned}: {calls:?}"
        );
    }
}
