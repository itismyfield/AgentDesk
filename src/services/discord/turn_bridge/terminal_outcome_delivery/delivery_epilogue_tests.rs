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
    cancel_token: Arc<crate::services::provider::CancelToken>,
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
            cancel_token: Arc::new(crate::services::provider::CancelToken::new()),
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

    /// Hand the marker to a different owner before the turn starts, so the
    /// production CAS is the thing that decides this bridge does not own it.
    fn with_marker_already_owned(self) -> Self {
        self.marker.store(true, Ordering::Release);
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
                cancel_token: Arc::clone(&self.cancel_token),
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

// ===========================================================================
// #5191 R2 S1-fix — witnesses for the pre-publish claim.
//
// The S1-prep commit on this branch pinned the OLD ordering: the bridge entered
// every publishing arm with `turn_delivered` still `false`, and only the
// epilogue's post-commit store set it. Those assertions are inverted here on
// purpose — that inversion IS the repair, and it is meant to be visible as an
// edit rather than as silence.
//
// LIMITS THIS SLICE DOES NOT CLOSE (design r3 §9). Do not read any green below
// as covering them:
//   L1  the empty-response recovery edit and the `silent_turn` commit resolve
//       BEFORE the claim, so their pre-store window is unchanged. `W-CONFIRM`
//       pins the current behaviour there, nothing more.
//   L2  handle ABA in the OTHER direction: after the claim, a registry
//       replacement gives the new watcher a fresh `false` marker, which this
//       bridge never claimed and never sets, so that watcher is not suppressed
//       and can still duplicate. `W-XCLAIM` is a `D1`-only witness; see its own
//       negative note.
//   L3  a fallback POST that actually published the body still reports "not
//       committed", so settle rolls the marker back. `W-FALLBACK` fixes that as
//       a known residue.
//   L4  a drop between the publish landing and the settle. Unchanged from
//       before this slice, so not a regression, but not closed either.
//   L5  the CAS only decides OWNERSHIP of the marker. Two concurrent bridges
//       are not mutually exclusive for publishing; the loser simply does not
//       own the rollback.
//   L6  A6 does not participate in the `DeliveryLeaseCell`, so sink/A6 mutual
//       exclusion is still absent. A6-versus-watcher duplication IS closed by
//       the claim — that half is not a residue.
//   L7  only the inline replace (A5) and the legacy long-chunk arm (A3) have
//       ordering witnesses. The other four arms are covered by the position of
//       the single `try_claim` statement, not by a driven test.
//   L8  FIXTURE COVERAGE GAP (not a design residue): the epilogue's `tv_done`
//       suspension is reachable in production but not from this driver, whose
//       synthetic message ids fall below the `is_real_discord_message_id`
//       floor. `W-POSTSTORE` lands on the stale-prefix `delete_message` drain
//       instead — the same span, a different point. See that witness for the
//       symbol-level derivation.
//
// ABSOLUTE LINE: a claim that survives a turn which did not deliver suppresses
// that watcher forever. Every witness below that asserts the marker is unset
// after an abnormal exit is guarding that, not tidiness.
// ===========================================================================

/// W4 — the inline terminal replace (A5) enters its publish with the marker
/// ALREADY claimed, and a committed turn keeps it.
///
/// This is the repair, stated positively: there is no longer an instant at
/// which the answer is on Discord while `turn_delivered` reads `false`.
///
/// Kills a `try_claim` that CASes without storing, and a `settle` that rolls
/// back unconditionally.
#[tokio::test]
async fn claim_marks_the_watcher_before_the_inline_terminal_publish_5191() {
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
        publishes[0].marker_at_entry,
        "the bridge must hold the watcher claim BEFORE it publishes"
    );
    assert!(
        driver.marker(),
        "a committed delivery keeps the watcher suppressed"
    );
    assert!(output.terminal_delivery_committed);
    assert!(!output.preserve_inflight_for_cleanup_retry);
}

/// W3 — the epilogue's own store is an idempotent confirm on a claimed turn.
///
/// The claim sets the marker, settle defuses without touching it, and the
/// epilogue store writes the same value again. The point is that nothing in
/// that sequence dips the marker back to `false`, so a watcher polling at any
/// moment after the claim sees a suppressed turn.
///
/// Kills a `settle` that rolls back unconditionally.
#[tokio::test]
async fn epilogue_store_is_an_idempotent_confirm_after_a_claim_5191() {
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    let (ctx, state) = driver.parts();
    let mut future = Box::pin(run_terminal_outcome_delivery(ctx, state));

    // Step the future one poll at a time and sample the marker between polls.
    // Once the claim has been taken, no intermediate state may show `false`.
    let mut claimed_yet = false;
    for _ in 0..DRIVER_POLL_BUDGET {
        let done = poll_at_most(&mut future, 1);
        if driver.marker() {
            claimed_yet = true;
        } else {
            assert!(
                !claimed_yet,
                "the marker dipped back to false between the claim and the epilogue store"
            );
        }
        if done {
            break;
        }
    }
    assert!(claimed_yet, "the turn must have claimed the marker at all");
    assert!(driver.marker(), "the confirmed turn stays suppressed");
}

/// W-OWN — a marker that is already `true` belongs to someone else, and this
/// bridge must neither take it nor clear it.
///
/// The turn below FAILS to deliver, so its settle would roll a claim back. It
/// must not, because it never won the CAS: clearing another owner's marker
/// un-suppresses a turn that owner already delivered, which is a duplicate.
///
/// Kills a `try_claim` that stores unconditionally instead of comparing.
#[tokio::test]
async fn claim_refuses_a_marker_another_owner_already_holds_5191() {
    let driver =
        TerminalDeliveryDriver::new(ReplaceBehaviour::Failed, 1).with_marker_already_owned();

    let (ctx, state) = driver.parts();
    let output = tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
        .await
        .expect("terminal outcome delivery must not hang");

    assert_eq!(driver.publish_entries().len(), 1);
    assert!(
        output.preserve_inflight_for_cleanup_retry,
        "the failed replace still preserves the turn for retry"
    );
    assert!(
        driver.marker(),
        "a bridge that lost the CAS must not clear the winner's marker"
    );
}

/// W2 — a headless (A6) turn whose delivery stays ambiguous must NOT leave the
/// watcher suppressed.
///
/// The claim is taken before the fork, so it is armed on this path too; the
/// settle has to give it back when the epilogue gate says the turn is preserved
/// for retry. If it did not, the answer would be undelivered AND the watcher
/// permanently silenced for it — the absolute-line failure.
///
/// Kills a `settle` that always defuses, and a
/// `bridge_epilogue_marks_watcher_delivered` that drops its `preserve` term.
#[tokio::test]
async fn ambiguous_headless_delivery_releases_the_claim_5191() {
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    let (mut ctx, state) = driver.parts();
    // A6: no local chaining, so delivery goes through the headless outbox. With
    // no PostgreSQL pool and no Discord http this resolves Ambiguous, which is
    // the production disposition for "we cannot confirm this was delivered".
    ctx.can_chain_locally = false;

    let output = tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
        .await
        .expect("terminal outcome delivery must not hang");

    assert!(
        driver.publish_entries().is_empty(),
        "the headless arm must not take the inline replace"
    );
    assert!(
        output.preserve_inflight_for_cleanup_retry,
        "an ambiguous headless delivery preserves the turn for retry"
    );
    assert!(
        !output.terminal_delivery_committed,
        "an ambiguous headless delivery is not a commit"
    );
    assert!(
        !driver.marker(),
        "an undelivered turn must never leave the watcher suppressed"
    );
}

/// W-CANCEL — a cancelled headless turn is a COMMIT, and the claim stays.
///
/// `turn_delivered = true` means "the bridge finished this turn", not "the
/// bridge posted something". A cancellation ends the turn, so the watcher must
/// not relay it afterwards. Pinning this stops a future reading of the marker
/// as "was published" from turning cancellations back into retries.
///
/// Kills a `headless_delivery_disposition` that maps `Cancelled` to
/// `PreserveForRetry`.
#[tokio::test]
async fn cancelled_headless_turn_keeps_the_claim_and_does_not_retry_5191() {
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    driver
        .cancel_token
        .publish_cancel("terminal-delivery-driver");
    let (mut ctx, state) = driver.parts();
    ctx.can_chain_locally = false;

    let output = tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
        .await
        .expect("terminal outcome delivery must not hang");

    assert!(
        output.terminal_delivery_committed,
        "a cancelled headless turn is committed, not retried"
    );
    assert!(
        !output.preserve_inflight_for_cleanup_retry,
        "a cancelled turn must not be preserved for retry"
    );
    assert!(
        driver.marker(),
        "a turn the bridge finished by cancelling must still suppress the watcher"
    );
}

/// W-CONFIRM — the epilogue store is the ONLY writer on a path that never
/// reaches the claim.
///
/// A fully consumed response commits before the publishing fork: there is
/// nothing left to send, so `try_claim` is never executed and the marker can
/// only come from the epilogue. Deleting that store would silently un-suppress
/// this whole family of turns.
///
/// This also delimits `L1`: it pins the CURRENT behaviour of that path, and
/// makes no claim that its own pre-store window is closed.
///
/// Kills a deleted `store(true)` under the epilogue's gate.
#[tokio::test]
async fn epilogue_store_is_the_only_writer_when_the_claim_is_never_reached_5191() {
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    let (ctx, mut state) = driver.parts();
    // Everything in `full_response` has already been sent, so the sink commits
    // the turn without entering the publishing fork.
    state.response_sent_offset = DRIVER_BODY.len();

    let output = tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
        .await
        .expect("terminal outcome delivery must not hang");

    let observed = driver.observations();
    // The epilogue's stale-prefix drain still runs; what must NOT happen is a
    // publish, because that is the fork the claim lives in front of.
    assert!(
        observed.iter().all(|call| call.call == DriverCall::Delete),
        "the fully-consumed sink publishes nothing; observed={observed:?}"
    );
    assert_eq!(
        driver.completed_publications(),
        0,
        "nothing was published on this path"
    );
    assert!(
        output.terminal_delivery_committed,
        "a fully consumed response is a commit"
    );
    assert!(
        driver.marker(),
        "with no claim on this path, the epilogue store is the only thing that can \
         suppress the watcher"
    );
}

/// W-P0 — a panic inside the publish rolls the claim back.
///
/// This is the reason the claim is a `Drop` type rather than a pair of
/// statements. The turn holds the marker when it enters the publish; the panic
/// unwinds past every explicit settle; and the watcher must still be free
/// afterwards, because nothing was delivered.
///
/// Kills a `Drop` body that does nothing.
#[tokio::test]
async fn publish_panic_rolls_the_claim_back_5191() {
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
    let publishes = driver.publish_entries();
    assert_eq!(publishes.len(), 1, "the unwind happens at the publish seam");
    assert!(
        publishes[0].marker_at_entry,
        "the claim was held when the publish was entered"
    );
    assert_eq!(
        driver.completed_publications(),
        0,
        "the panicking publish never landed"
    );
    assert!(
        !driver.marker(),
        "an unwound turn must never leave the watcher suppressed"
    );
    drop(future);
}

/// W-P0b — dropping the bridge future mid-publish rolls the claim back.
///
/// A cancelled bridge task does not panic; its future is simply dropped. That
/// exit has to restore the marker for the same reason a panic does, and it is
/// reached through the same `Drop`.
///
/// Kills a `Drop` body that does nothing.
#[tokio::test]
async fn dropping_the_bridge_future_mid_publish_rolls_the_claim_back_5191() {
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    let (ctx, state) = driver.parts();
    let mut future = Box::pin(run_terminal_outcome_delivery(ctx, state));

    // One poll enters the publish and suspends inside it.
    assert!(
        !poll_at_most(&mut future, 1),
        "the publish must still be in flight after one poll"
    );
    let publishes = driver.publish_entries();
    assert_eq!(publishes.len(), 1, "the publish was entered");
    assert!(
        publishes[0].marker_at_entry,
        "the claim was held when the publish was entered"
    );
    assert_eq!(
        driver.completed_publications(),
        0,
        "the publish has not landed yet"
    );
    assert!(driver.marker(), "the claim is live while the publish runs");

    drop(future);

    assert!(
        !driver.marker(),
        "a dropped turn whose publish never landed must free the watcher"
    );
}

/// W-POSTSTORE — once the gateway has actually published, NO drop point may
/// take the marker back.
///
/// This is the witness that pins `settle` to the near side of the epilogue.
/// Settling afterwards leaves the rollback guard armed across the epilogue's
/// own awaits, so a drop there would clear the marker for an answer that is
/// already on Discord and reopen the duplicate window.
///
/// ENTRY IS NOT SUCCESS. The fake gateway records reaching the gateway method,
/// then suspends, and only resolves the outcome on a later poll. A drop at the
/// entering poll is therefore NOT a post-success drop — nothing was delivered
/// there and rolling back is correct. Every assertion below is built on
/// `completed_publications`, never on the entry observations.
///
/// Design r3 §1.3 names `tv_done` as the window this sweep lands in. This
/// fixture DOES NOT COVER THAT WINDOW — and that is a coverage gap in the
/// fixture, not an error in the design. In production `tv_done` really does
/// suspend after the publish:
///
/// * `delivery_epilogue.rs` calls `tv_done` under
///   `can_chain_locally && !preserve_inflight_for_cleanup_retry &&
///   !delivery_response.trim().is_empty() && user_msg_id.is_some()`, all of
///   which a normal locally-chained terminal answer satisfies;
/// * `tv_done` is `note_intake_turn_completed_via_shared` ->
///   `TurnViewReconciler::note_turn_completed` -> `note_state` ->
///   `note_state_delivery_with_clear_attempt_guard`, which returns early ONLY
///   when `!is_real_discord_message_id(target.message_id)`;
/// * that predicate is the half-open range
///   `100_000_000_000_000..9_000_000_000_000_000_000`, and real Discord
///   snowflakes (~1.4e18) fall inside it, so production does NOT short-circuit
///   and reaches `target_lock.lock().await` and the reaction work past it.
///
/// The driver cannot get there: its synthetic ids are far below the floor, so
/// with `user_msg_id = Some(..)` the call short-circuits before its first
/// await, and a real-shaped id would instead route the epilogue into the
/// voice-completion block, whose `spawn_blocking` config load a spin-polled
/// task cannot resolve (see the MEASURED CONSTRAINT below). So this sweep
/// lands on the other reachable post-publish suspension inside the same
/// epilogue: the stale-prefix `delete_message` drain. Same standing — it sits
/// inside the span a late settle would keep armed — so the invariant and the
/// mutant it kills are unchanged, but the `tv_done` suspension itself has no
/// witness here and is recorded as NOT CLOSED.
///
/// MEASURED CONSTRAINT: a spin-polled task cannot resolve the epilogue's
/// voice-completion config load (`cached_config` awaits a `spawn_blocking`), so
/// the sweep drives the production shape that skips it — a turn with no
/// anchored user message, which the epilogue documents as a real recovery
/// shape. The publish, the drain and the marker store all still run.
///
/// Kills a `settle` moved behind `handle_delivery_epilogue`, and a `Drop` body
/// that does nothing.
#[tokio::test]
async fn published_answers_survive_every_drop_point_5191() {
    let mut table: Vec<(usize, bool, usize, bool)> = Vec::new();
    let mut polls_to_complete = None;
    for polls in 0..DRIVER_POLL_BUDGET {
        // Two suspensions per gateway call so the sweep has several interior
        // landing points rather than exactly one.
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 2);
        let (mut ctx, state) = driver.parts();
        ctx.user_msg_id = None;
        let mut future = Box::pin(run_terminal_outcome_delivery(ctx, state));
        let completed = poll_at_most(&mut future, polls);
        drop(future);

        let landed = driver.completed_publications();
        let marker = driver.marker();
        table.push((polls, completed, landed, marker));
        if landed > 0 {
            assert!(
                marker,
                "a published answer was un-suppressed by a drop at poll {polls} \
                 (publications_landed={landed}, completed={completed})"
            );
        }
        if completed {
            polls_to_complete = Some(polls);
            break;
        }
    }
    let polls_to_complete =
        polls_to_complete.expect("the driven future must complete inside the sweep's poll budget");

    // Measured shape, not a floor. The derivation, so a reviewer can redo it:
    //   * this sweep constructs the driver with `yields_per_call = 2`, so every
    //     gateway future returns `Pending` twice before it resolves;
    //   * `poll_at_most(&mut future, polls)` polls exactly `polls` times and
    //     then the future is dropped, so row `polls` is "drop after N polls";
    //   * poll 1 enters the replace and suspends, poll 2 suspends again, poll 3
    //     resolves it — `completed_publications` becomes 1 on poll 3;
    //   * the epilogue's stale-prefix `delete_message` drain then suspends
    //     twice the same way and resolves on poll 5, which is also where the
    //     whole future completes;
    //   * so the rows with `landed > 0 && !completed` are exactly polls 3 and 4:
    //     two post-success drop points.
    // The S1-prep sweep on the same code path reports 1 instead, because it
    // builds its driver with `yields_per_call = 1`.
    // Pinning the exact number keeps the sweep from silently degrading into a
    // witness that never actually lands after a success — which is how the
    // entry-versus-success conflation this suite previously carried went
    // unnoticed.
    let post_success_drop_points = table
        .iter()
        .filter(|(_, completed, landed, _)| !*completed && *landed > 0)
        .count();
    assert_eq!(
        post_success_drop_points, 2,
        "the sweep must land after a publish RESOLVED, twice for this fixture; \
         table={table:?}"
    );
    assert!(
        polls_to_complete >= 3,
        "the sweep needs interior suspension points to be a witness at all, but the \
         future completed in {polls_to_complete} polls"
    );
}

/// W4b — the legacy long-chunk arm (A3) also publishes under the claim.
///
/// A claim placed correctly for the inline replace says nothing about the other
/// arms. This drives the second one so that moving the `try_claim` statement
/// past a publishing arm cannot pass unnoticed.
///
/// Kills a `try_claim` call moved down to just before the inline replace.
#[tokio::test]
async fn claim_marks_the_watcher_before_the_legacy_long_chunk_publish_5191() {
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
        observed
            .iter()
            .filter(|call| call.call == DriverCall::Send)
            .all(|call| call.marker_at_entry),
        "every chunk of the long-chunk arm must be sent under the claim; \
         observed={observed:?}"
    );
    assert!(output.terminal_delivery_committed);
    assert!(driver.marker());
}

/// W-XCLAIM — `D1` ONLY: the claim owns the `Arc` it CASed and never
/// re-resolves the channel key, so replacing the registry entry mid-turn cannot
/// make this bridge write to the replacement's marker.
///
/// NEGATIVE NOTE, REQUIRED READING: this green is NOT coverage of the opposite
/// direction. `D2` — the replacement watcher reads its own fresh `false` marker
/// and relays the same answer anyway — is NOT closed by this slice and is not
/// tested anywhere. Closing it needs a generation-transfer or
/// registry-replacement protocol, which is a separate axis. See `L2`. Anyone
/// reading this test as "handle replacement is handled" is reading it wrong.
///
/// Kills a claim that stores the channel key and re-resolves it at settle time.
#[tokio::test]
async fn claim_never_writes_to_a_replacement_watchers_marker_5191() {
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Failed, 1);
    let (ctx, state) = driver.parts();
    let mut future = Box::pin(run_terminal_outcome_delivery(ctx, state));

    // Enter the publish so the claim is live, then swap the registry entry for
    // a different watcher with its own, independent marker.
    assert!(!poll_at_most(&mut future, 1));
    assert!(driver.marker(), "the claim is live");
    let replacement = Arc::new(AtomicBool::new(true));
    driver.shared.tmux_watchers.insert(
        ChannelId::new(DRIVER_CHANNEL_ID),
        TmuxWatcherHandle {
            tmux_session_name: format!("{DRIVER_TMUX_SESSION}-replacement"),
            output_path: "replacement.jsonl".to_string(),
            paused: Arc::new(AtomicBool::new(false)),
            resume_offset: Arc::new(Mutex::new(None)),
            cancel: Arc::new(AtomicBool::new(false)),
            pause_epoch: Arc::new(AtomicU64::new(0)),
            turn_delivered: Arc::clone(&replacement),
            last_heartbeat_ts_ms: Arc::new(AtomicI64::new(
                crate::services::discord::tmux_watcher_registry::tmux_watcher_now_ms(),
            )),
        },
    );

    assert!(
        poll_at_most(&mut future, DRIVER_POLL_BUDGET),
        "the driven future must complete inside the poll budget"
    );

    assert!(
        !driver.marker(),
        "the failed turn rolls back the marker it actually claimed"
    );
    assert!(
        replacement.load(Ordering::Acquire),
        "the replacement watcher's own marker must be untouched (D1)"
    );
}

/// W-FALLBACK — `L3`, pinned as a KNOWN RESIDUE, not as a fix.
///
/// `SentFallbackAfterEditFailure` means the in-place edit failed but a fallback
/// POST did publish the body. The existing contract classifies that as "not
/// committed" (there is a regression test in `terminal_delivery` that fails if
/// the predicate ever starts committing it), so the turn is preserved for retry
/// and settle gives the marker back — for an answer that IS on Discord.
///
/// THIS SLICE DOES NOT CLOSE THAT. The behaviour is identical to before the
/// claim existed, so it is not a regression, but a duplicate reproduced through
/// this path is `L3` and not a failure of the claim. Closing it needs a
/// separate "the body was observed to land" signal threaded out of the
/// publishing arms, which is a different axis.
///
/// When that signal arrives this test is expected to FAIL, and that failure is
/// the intended notification that the contract changed.
#[tokio::test]
async fn fallback_post_that_published_still_releases_the_claim_5191_l3() {
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::FallbackAfterEditFailure, 1);
    let (ctx, state) = driver.parts();
    let output = tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
        .await
        .expect("terminal outcome delivery must not hang");

    assert_eq!(
        driver.completed_publications(),
        1,
        "the fallback POST landed"
    );
    assert!(
        output.preserve_inflight_for_cleanup_retry,
        "the existing contract does not commit a fallback-after-edit-failure"
    );
    assert!(
        !driver.marker(),
        "KNOWN RESIDUE (L3): the body was published, yet the claim is released, \
         so the watcher can still relay it. This slice does not close that."
    );
}
