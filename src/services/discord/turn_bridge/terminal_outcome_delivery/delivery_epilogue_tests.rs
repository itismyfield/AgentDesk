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
use tracing_subscriber::fmt::MakeWriter;

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
            terminal_body_visible: true,
            preserve_inflight_for_cleanup_retry: false,
            should_complete_work_dispatch_after_delivery: false,
            should_fail_dispatch_after_delivery: false,
            bridge_relay_delegated_to_watcher: false,
            watcher_owner_channel_id: channel_id,
            can_chain_locally: false,
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
    marker: Arc<AtomicBool>,
    observations: Arc<Mutex<Vec<DriverObservation>>>,
    /// Bumped only after a publishing call has RESOLVED to a success outcome —
    /// i.e. after the suspension, at the point the production code would learn
    /// the answer is on Discord. Kept apart from the entry observations above
    /// because the two are true at different polls, and conflating them
    /// overstates by exactly one suspension what the drop sweep has witnessed.
    completed_publications: Arc<AtomicUsize>,
    replace: ReplaceBehaviour,
    yields_per_call: usize,
}

impl DriverGateway {
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
        _channel_id: ChannelId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        self.observe(DriverCall::Send);
        let yields = self.yields_per_call;
        let completed = Arc::clone(&self.completed_publications);
        Box::pin(async move {
            Yields(yields).await;
            completed.fetch_add(1, Ordering::Release);
            Ok(MessageId::new(DRIVER_FALLBACK_ANCHOR_MSG_ID))
        })
    }

    fn edit_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        self.observe(DriverCall::Edit);
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
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        self.observe(DriverCall::Replace);
        let (yields, behaviour) = (self.yields_per_call, self.replace);
        let completed = Arc::clone(&self.completed_publications);
        Box::pin(async move {
            Yields(yields).await;
            match behaviour {
                ReplaceBehaviour::Edited => {
                    completed.fetch_add(1, Ordering::Release);
                    Ok(ReplaceLongMessageOutcome::EditedOriginal)
                }
                ReplaceBehaviour::FallbackAfterEditFailure => {
                    completed.fetch_add(1, Ordering::Release);
                    Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                        edit_error: "edit 500; fallback POST succeeded".to_string(),
                        replacement_anchor: Some(MessageId::new(DRIVER_FALLBACK_ANCHOR_MSG_ID)),
                    })
                }
                ReplaceBehaviour::Failed => Err("driver terminal replace failed".to_string()),
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
        true
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
        let gateway: Arc<dyn TurnGateway> = Arc::new(DriverGateway {
            marker: Arc::clone(&marker),
            observations: Arc::clone(&observations),
            completed_publications: Arc::clone(&completed_publications),
            replace,
            yields_per_call,
        });

        Self {
            shared,
            gateway,
            marker,
            observations,
            completed_publications,
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
                can_chain_locally: true,
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
