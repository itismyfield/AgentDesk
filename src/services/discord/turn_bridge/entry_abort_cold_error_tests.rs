//! A cold Herdr turn whose actual executor `Err` reaches the bridge through intake's own
//! conversion, so one trace decides the policy close, or the hold without a before-start proof.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use crate::services::discord::formatting::ReplaceLongMessageOutcome;
use crate::services::discord::gateway::GatewayFuture;
use crate::services::discord::recovery_engine::herdr_admitted_restart::ADMITTED_ABORT_NOTICE;
use crate::services::discord::turn_bridge::before_start_stop::test_hooks;
use crate::services::provider::cancel_token_claude_interrupt::HerdrSubmission;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

/// Every body the bridge showed on Discord, in order; a send never awaited shows nothing.
#[derive(Default)]
pub(in crate::services::discord::turn_bridge) struct Shown(
    pub(in crate::services::discord::turn_bridge) Mutex<Vec<String>>,
);

impl Shown {
    fn bodies(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl TurnGateway for Shown {
    fn send_message<'a>(
        &'a self,
        _: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(content.to_owned());
            Ok(MessageId::new(5_340_990))
        })
    }
    fn edit_message<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(content.to_owned());
            Ok(())
        })
    }
    fn replace_message_with_outcome<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(content.to_owned());
            Ok(ReplaceLongMessageOutcome::EditedOriginal)
        })
    }
    fn schedule_retry_with_history<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async {})
    }
    fn dispatch_queued_turn<'a>(
        &'a self,
        _: ChannelId,
        _: &'a discord::Intervention,
        _: &'a str,
        _: bool,
        _: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn validate_live_routing<'a>(&'a self, _: ChannelId) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn requester_mention(&self) -> Option<String> {
        None
    }
    fn can_chain_locally(&self) -> bool {
        false
    }
    fn can_deliver_directly(&self) -> bool {
        true
    }
    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        None
    }
}

/// The executor's ports with no launch host; each request for one is counted.
#[derive(Default)]
struct Launches(std::sync::atomic::AtomicUsize);

impl Launches {
    fn host(&self) -> Option<Arc<dyn crate::services::herdr_launch::HerdrLaunchHost>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        None
    }
}

impl crate::services::claude::herdr_turn::HerdrTurnPorts for Launches {
    fn launch_host(&self) -> Option<Arc<dyn crate::services::herdr_launch::HerdrLaunchHost>> {
        self.host()
    }
    fn hook_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::services::claude_tui::hook_server::HookEvent> {
        tokio::sync::broadcast::channel(1).1
    }
    fn attach(
        &self,
        _: &crate::services::claude::herdr_turn::AttachRequest<'_>,
    ) -> Result<bool, String> {
        panic!("no attach")
    }
    fn confirm_bound(
        &self,
        _: &HostedOwner,
        _: &crate::db::dispatched_sessions::hosted_execution::HostedExecution,
        _: &crate::services::session_host::HerdrTarget,
    ) -> Result<(), String> {
        panic!("no bound")
    }
}

impl crate::services::codex::herdr_turn::CodexHerdrPorts for Launches {
    fn launch_host(&self) -> Option<Arc<dyn crate::services::herdr_launch::HerdrLaunchHost>> {
        self.host()
    }
    fn attach(
        &self,
        _: &HostedOwner,
        _: &crate::db::dispatched_sessions::hosted_execution::HostedExecution,
        _: &crate::services::tui_prompt_dedupe::binding_events::SourceId,
        _: &crate::services::session_host::HerdrTarget,
    ) -> Result<bool, String> {
        panic!("no attach")
    }
    fn confirm_bound(
        &self,
        _: &HostedOwner,
        _: &crate::db::dispatched_sessions::hosted_execution::HostedExecution,
        _: &crate::services::session_host::HerdrTarget,
    ) -> Result<(), String> {
        panic!("no bound")
    }
}

/// The executor's error once it asked for the launch host it lacks.
const NO_LAUNCH_HOST: &str = "herdr turn: no launch host";

/// A Codex CLI advertising hooks and daemon isolation, with a published hook endpoint, so the
/// Codex launch preflight admits the turn and only the missing launch host refuses it.
struct CodexCli {
    _endpoint: crate::services::claude_tui::hook_server::HookEndpointGuard,
    _env: Vec<crate::config::test_env::TestEnvVarGuard>,
    _dir: tempfile::TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}

/// Needs the shared env lock already held, as `set_agentdesk_root_for_test` takes it.
fn codex_cli() -> CodexCli {
    use crate::config::test_env::TestEnvVarGuard as Guard;
    let lock = crate::services::claude_tui::hook_server::tests::ENDPOINT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let codex = dir.path().join("codex");
    let stub = "#!/bin/bash\nif [ \"$1\" = --version ]; then echo 'codex-cli 0.160.0'\n\
        elif [ \"$1 $2\" = 'resume --help' ]; then echo '--dangerously-bypass-hook-trust --no-daemon'\n\
        fi\n";
    std::fs::write(&codex, stub).unwrap();
    let executable = std::os::unix::fs::PermissionsExt::from_mode(0o700);
    std::fs::set_permissions(&codex, executable).unwrap();
    let env = vec![
        Guard::set_path_after_shared_test_env_lock("AGENTDESK_CODEX_PATH", &codex),
        Guard::set_path_after_shared_test_env_lock("CODEX_HOME", dir.path()),
        Guard::set_value_after_shared_test_env_lock(
            "AGENTDESK_CODEX_DIRECT_TUI_HOOKS",
            "1".as_ref(),
        ),
    ];
    let endpoint = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:9".into(),
    );
    CodexCli {
        _endpoint: endpoint,
        _env: env,
        _dir: dir,
        _lock: lock,
    }
}

/// One cold turn as intake leaves it: its actor holds the mailbox, its row is saved, and one
/// active turn is counted.
struct Cold {
    provider: ProviderKind,
    shared: Arc<discord::SharedData>,
    channel: ChannelId,
    cancel: Arc<CancelToken>,
    owner: HostedOwner,
    row: InflightTurnState,
    shown: Arc<Shown>,
}

fn owner(shared: &discord::SharedData, provider: &ProviderKind, channel: u64) -> HostedOwner {
    HostedOwner {
        provider: provider.as_str().into(),
        discord_token_hash: shared.token_hash.clone(),
        channel_id: channel.to_string(),
        logical_key: format!("AgentDesk-{}-cold-error-{channel}", provider.as_str()),
        owner_node: "node".into(),
        runtime_root: "/tmp".into(),
    }
}

async fn cold(provider: ProviderKind, channel_id: u64) -> Cold {
    let mut shared = discord::make_shared_data_for_tests();
    let writable = Arc::get_mut(&mut shared).unwrap();
    writable.provider = provider.clone();
    // A booted process generation: the policy receipt records it.
    writable.restart.current_generation = 17;
    let channel = ChannelId::new(channel_id);
    let cancel = Arc::new(CancelToken::new());
    let owner = owner(&shared, &provider, channel_id);
    let mut row = InflightTurnState::new(
        provider.clone(),
        channel_id,
        None,
        1,
        77_200,
        18,
        String::new(),
        None,
        Some(owner.logical_key.clone()),
        None,
        None,
        0,
    );
    row.turn_nonce = cancel.turn_nonce().map(str::to_owned);
    cancel.prepare_herdr_interrupt(provider.clone(), &owner);
    let message = MessageId::new(row.user_msg_id);
    let start =
        discord::mailbox_try_start_turn(&shared, channel, cancel.clone(), UserId::new(1), message);
    assert!(start.await);
    discord::increment_global_active(&shared, "test_bridge_admission");
    discord::inflight::save_inflight_state(&row).unwrap();
    let shown = Arc::new(Shown::default());
    Cold {
        provider,
        shared,
        channel,
        cancel,
        owner,
        row,
        shown,
    }
}

impl Cold {
    /// The actual provider executor with no launch host, on its blocking thread, as `owner`;
    /// returns its error and how often it asked for a launch host.
    async fn execute(&self, owner: HostedOwner) -> (Result<(), String>, usize) {
        let (actor, provider) = (self.cancel.clone(), self.provider.clone());
        let launches = Arc::new(Launches::default());
        let ports = launches.clone();
        let result = tokio::task::spawn_blocking(move || {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
                .unwrap();
            let endpoint = crate::services::herdr_launch::HerdrLaunchEndpoint {
                execution_node: "test-node".into(),
                config_key: "unused".into(),
                socket_addr: "unused".into(),
                herdr_session: "unused".into(),
            };
            let (sender, _) = std::sync::mpsc::channel();
            let channel_id = owner.channel_id.parse().unwrap();
            if provider == ProviderKind::Claude {
                let turn = crate::services::claude::herdr_turn::HerdrTurn {
                    pool: &pool,
                    channel_id,
                    owner,
                    endpoint,
                    row: None,
                    prompt: "cold",
                    working_dir: ".",
                    system_prompt: None,
                    model: None,
                    hook_endpoint: None,
                    cancel: Some(actor),
                };
                crate::services::claude::herdr_turn::execute(turn, ports.as_ref(), sender)
            } else {
                let turn = crate::services::codex::herdr_turn::CodexHerdrTurn {
                    pool: &pool,
                    channel_id,
                    owner,
                    endpoint,
                    row: None,
                    prompt: "cold",
                    working_dir: ".",
                    system_prompt: None,
                    model: None,
                    allowed_tools: &[],
                    fast_mode: None,
                    goals: None,
                    compact_token_limit: None,
                    cancel: Some(actor),
                };
                crate::services::codex::herdr_turn::execute(turn, ports.as_ref(), sender)
            }
        })
        .await
        .unwrap();
        (result, launches.0.load(Ordering::SeqCst))
    }

    fn stop(&self) {
        let state = self.cancel.herdr_interrupt_state().unwrap();
        state.user_stop.store(true, Ordering::Release);
    }

    /// Converts the executor's `Err` as intake does, sends what that yields and runs the bridge
    /// to its exit; returns the completion signal and whether the actor still holds the mailbox.
    async fn bridge(&self, error: String) -> (BridgeCompletionSignal, bool) {
        let channel = self.channel.get();
        let frame =
            crate::services::discord::router::message_handler::producer_error_frame(channel, error);
        self.bridge_frames(frame).await
    }

    /// The bridge over the executor's stream holding `frame`, if any, run to its exit.
    async fn bridge_frames(&self, frame: Option<StreamMessage>) -> (BridgeCompletionSignal, bool) {
        let mut bridge = seed_context("", self.row.clone());
        bridge.provider = self.provider.clone();
        bridge.gateway = self.shown.clone();
        bridge.user_msg_id = Some(MessageId::new(self.row.user_msg_id));
        bridge.current_msg_id = Some(MessageId::new(18));
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        bridge.completion_tx = Some(completion_tx);
        let (tx, rx) = mpsc::channel();
        if let Some(frame) = frame {
            tx.send(frame).unwrap();
        }
        drop(tx);
        spawn_turn_bridge(self.shared.clone(), self.cancel.clone(), rx, bridge);
        let signal = tokio::time::timeout(std::time::Duration::from_secs(20), completion_rx)
            .await
            .expect("the bridge reports")
            .unwrap();
        let settle = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut holds = true;
        while std::time::Instant::now() < settle {
            holds = self.holds(&self.cancel).await;
            if !holds {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        (signal, holds)
    }

    async fn holds(&self, actor: &Arc<CancelToken>) -> bool {
        let snapshot = discord::mailbox_snapshot(&self.shared, self.channel).await;
        snapshot
            .cancel_token
            .as_ref()
            .is_some_and(|token| Arc::ptr_eq(token, actor))
    }

    fn durable(&self) -> Option<InflightTurnState> {
        discord::inflight::load_inflight_state(&self.provider, self.channel.get())
    }

    fn row_path(&self) -> std::path::PathBuf {
        let root = discord::inflight::inflight_runtime_root().unwrap();
        discord::inflight::inflight_state_path(&root, &self.provider, self.channel.get())
    }

    fn row_bytes(&self) -> Option<Vec<u8>> {
        std::fs::read(self.row_path()).ok()
    }

    /// Whether the bridge took the Error frame: its durable row carries the bridge's failure body
    /// quoting the executor's own error.
    fn relayed(row: Option<&InflightTurnState>, error: &str) -> bool {
        row.is_some_and(|row| row.full_response.contains(&format!("Error: {error}")))
    }

    fn active(&self) -> usize {
        self.shared.restart.global_active.load(Ordering::Relaxed)
    }
}

/// Each `consume` step the bridge reached, with the durable receipt's notice still pending, and
/// whether the durable row then carried the bridge's body for `error`.
type Steps = std::rc::Rc<std::cell::RefCell<Vec<(&'static str, Option<bool>, bool)>>>;

fn observe_receipt(provider: ProviderKind, error: String) -> Steps {
    let steps = Steps::default();
    let seen = steps.clone();
    let receipt = move |step: &'static str, row: &InflightTurnState| {
        let durable = discord::inflight::load_inflight_state(&provider, row.channel_id);
        let relayed = Cold::relayed(durable.as_ref(), &error);
        let pending = durable
            .and_then(|row| row.herdr_before_start_stop)
            .map(|receipt| receipt.notice_pending());
        seen.borrow_mut().push((step, pending, relayed));
    };
    test_hooks::observe(Some(Box::new(receipt)));
    steps
}

/// The provider's own launch failure, before any input, then the user's stop: the bridge closes
/// the turn by policy from that Error frame, one notice, one receipt, and frees what it held.
#[tokio::test(flavor = "current_thread")]
async fn cold_launch_error_reaches_bridge_policy_close_both_providers() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    // The bridge owns these bodies; the policy close shows its stop only there.
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let _codex = codex_cli();
    for (n, provider) in [ProviderKind::Claude, ProviderKind::Codex]
        .into_iter()
        .enumerate()
    {
        let turn = cold(provider.clone(), 5_340_661_000 + n as u64).await;
        let (result, launches) = turn.execute(turn.owner.clone()).await;
        // Each executor passed its preflight and failed only at the launch host it asked for.
        assert_eq!(launches, 1, "{provider:?}: {result:?}");
        let error = result.unwrap_err();
        assert_eq!(error, NO_LAUNCH_HOST, "{provider:?}");
        turn.stop();
        let state = turn.cancel.herdr_interrupt_state().unwrap();
        assert_eq!(state.closed_probe(), Some(false), "{provider:?}: {error}");
        let steps = observe_receipt(provider.clone(), error.clone());
        let submitted = test_hooks::submitted();
        let (signal, holds) = turn.bridge(error.clone()).await;
        test_hooks::observe(None);
        let case = format!("{provider:?}: {error}");
        // The receipt goes Pending then Delivered on a row carrying the frame's error, and the
        // policy close's mailbox step clears it before releasing the turn.
        assert_eq!(
            *steps.borrow(),
            [
                ("before_receipt", None, true),
                ("after_receipt", Some(true), true),
                ("after_notice_ack", Some(false), true),
                ("before_submit", Some(false), true),
                ("after_submit", None, false),
            ],
            "{case}"
        );
        assert_eq!(test_hooks::submitted() - submitted, 1, "{case}");
        assert_eq!(turn.shown.bodies(), [ADMITTED_ABORT_NOTICE], "{case}");
        // No input reached a pane: the close rests on an unsubmitted, closed observation.
        assert_eq!(state.closed_probe(), Some(true), "{case}");
        assert_eq!(signal, BridgeCompletionSignal::Unresolved, "{case}");
        assert!(!holds, "{case}");
        assert!(turn.durable().is_none(), "{case}");
        assert_eq!(turn.active(), 0, "{case}");
    }
}

/// The prelaunch refusal of an executor whose token already holds another turn's stop state.
const PRELAUNCH_FOREIGN_STOP: &str = "herdr turn: the token holds another turn's stop state";

/// How a failed cold turn lacks a before-start proof.
#[derive(Clone, Copy, Debug)]
enum Unproven {
    /// The executor refused before observing any execution: the token holds another turn's stop.
    NoExecution,
    /// Input was submitted, or its submission is unknown, before the executor failed.
    Input(HerdrSubmission),
}

/// The same executor failure and stop without a proof keeps everything held: no policy close,
/// notice or receipt, the row and token stay, and the active turn is still counted.
#[tokio::test(flavor = "current_thread")]
async fn cold_error_without_proof_remains_held() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let _codex = codex_cli();
    let cases = [
        Unproven::NoExecution,
        Unproven::Input(HerdrSubmission::Submitted),
        Unproven::Input(HerdrSubmission::Unknown),
    ];
    let providers = [ProviderKind::Claude, ProviderKind::Codex];
    for (n, (provider, unproven)) in providers
        .iter()
        .flat_map(|provider| cases.map(|case| (provider.clone(), case)))
        .enumerate()
    {
        let turn = cold(provider.clone(), 5_340_662_000 + n as u64).await;
        let state = turn.cancel.herdr_interrupt_state().unwrap();
        let executor = match unproven {
            Unproven::NoExecution => {
                let mut other = turn.owner.clone();
                other.logical_key.push_str("-other");
                other
            }
            Unproven::Input(submission) => {
                let mut input = state.submission.lock().unwrap();
                assert!(state.prepare_input(&mut input));
                state.finish_input(&mut input, submission, false);
                turn.owner.clone()
            }
        };
        let (result, launches) = turn.execute(executor).await;
        // A refusal before launch never asks for a host; an executor past its preflight does.
        let (asked, expected) = match unproven {
            Unproven::NoExecution => (0, PRELAUNCH_FOREIGN_STOP),
            Unproven::Input(_) => (1, NO_LAUNCH_HOST),
        };
        assert_eq!(
            (launches, result.as_ref().err().map(String::as_str)),
            (asked, Some(expected))
        );
        let error = result.unwrap_err();
        turn.stop();
        let steps = observe_receipt(provider.clone(), error.clone());
        let submitted = test_hooks::submitted();
        let (signal, holds) = turn.bridge(error.clone()).await;
        test_hooks::observe(None);
        let case = format!("{provider:?} {unproven:?}: {error}");
        assert!(steps.borrow().is_empty(), "{case}: {:?}", steps.borrow());
        assert_eq!(test_hooks::submitted() - submitted, 0, "{case}");
        assert!(
            turn.shown.bodies().is_empty(),
            "{case}: {:?}",
            turn.shown.bodies()
        );
        let durable = turn.durable().expect("the held row stays");
        assert!(durable.herdr_before_start_stop.is_none(), "{case}");
        assert_eq!(durable.turn_nonce, turn.row.turn_nonce, "{case}");
        assert_eq!(durable.user_msg_id, turn.row.user_msg_id, "{case}");
        assert!(
            Cold::relayed(Some(&durable), &error),
            "{case}: {}",
            durable.full_response
        );
        assert_eq!(signal, BridgeCompletionSignal::Unresolved, "{case}");
        assert!(holds, "{case}");
        assert!(!turn.cancel.cancelled.load(Ordering::SeqCst), "{case}");
        assert_eq!(turn.active(), 1, "{case}");
    }
}

/// What took the channel before the failed turn's bridge settled.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Successor {
    /// A successor's row replaced the durable row after the bridge passed its mailbox check.
    Row,
    /// A successor's actor took the mailbox, with its own row, before the bridge started.
    Actor,
}

/// A failed cold turn's policy close never settles over a successor: its row bytes, its token
/// and the active count stay exactly as the successor left them.
#[tokio::test(flavor = "current_thread")]
async fn cold_error_does_not_clear_successor() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let _codex = codex_cli();
    for (n, successor) in [Successor::Row, Successor::Actor].into_iter().enumerate() {
        let turn = cold(ProviderKind::Codex, 5_340_663_000 + n as u64).await;
        let (result, launches) = turn.execute(turn.owner.clone()).await;
        assert_eq!(
            (launches, result.as_ref().err().map(String::as_str)),
            (1, Some(NO_LAUNCH_HOST))
        );
        let error = result.unwrap_err();
        turn.stop();
        let next = Arc::new(CancelToken::new());
        let mut row = turn.row.clone();
        row.turn_nonce = next.turn_nonce().map(str::to_owned);
        row.user_msg_id = 77_201;
        row.current_msg_id = 19;
        let written = std::rc::Rc::new(std::cell::RefCell::new(None));
        if successor == Successor::Actor {
            discord::mailbox_finish_turn(&turn.shared, &turn.provider, turn.channel).await;
            let message = MessageId::new(row.user_msg_id);
            let start = discord::mailbox_try_start_turn(
                &turn.shared,
                turn.channel,
                next.clone(),
                UserId::new(2),
                message,
            );
            assert!(start.await, "{successor:?}");
            discord::inflight::save_inflight_state(&row).unwrap();
            *written.borrow_mut() = turn.row_bytes();
        }
        let steps = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let (seen, replaced, bytes) = (steps.clone(), written.clone(), turn.row_path());
        test_hooks::observe(Some(Box::new(move |step, _: &InflightTurnState| {
            seen.borrow_mut().push(step);
            if step == "before_receipt" && replaced.borrow().is_none() {
                discord::inflight::save_inflight_state(&row).unwrap();
                *replaced.borrow_mut() = std::fs::read(&bytes).ok();
            }
        })));
        let active = turn.active();
        let submitted = test_hooks::submitted();
        let (signal, _) = turn.bridge(error).await;
        test_hooks::observe(None);
        let expected = match successor {
            Successor::Row => &["before_receipt"][..],
            Successor::Actor => &[][..],
        };
        assert_eq!(*steps.borrow(), expected, "{successor:?}");
        assert!(written.borrow().is_some(), "{successor:?}");
        assert_eq!(turn.row_bytes(), *written.borrow(), "{successor:?}");
        assert_eq!(test_hooks::submitted() - submitted, 0, "{successor:?}");
        // No stop shows; a bridge refused at entry for the successor's row says so on its own body.
        let bodies = turn.shown.bodies();
        let stop = bodies
            .iter()
            .any(|body| body.contains(ADMITTED_ABORT_NOTICE));
        assert!(!stop, "{successor:?}: {bodies:?}");
        assert!(
            successor == Successor::Actor || bodies.is_empty(),
            "{successor:?}: {bodies:?}"
        );
        // A refused close leaves the turn unresolved; a bridge refused at entry aborts there.
        let (holder, exit) = match successor {
            Successor::Row => (&turn.cancel, BridgeCompletionSignal::Unresolved),
            Successor::Actor => (&next, BridgeCompletionSignal::EntryAborted),
        };
        // The holder keeps the mailbox uncancelled and the active count is untouched.
        let kept = (
            signal,
            turn.holds(holder).await,
            holder.cancelled.load(Ordering::SeqCst),
            turn.active(),
        );
        let expected = (exit, true, false, active);
        assert_eq!(kept, expected, "{successor:?}");
    }
}

/// A submitted turn whose executor failed with no stop ends as any failed turn does: its error is
/// shown, its row and mailbox are released, and the channel takes its next input.
#[tokio::test(flavor = "current_thread")]
async fn a_submitted_turn_whose_executor_failed_without_a_stop_ends_normally() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let _codex = codex_cli();
    for (n, provider) in [ProviderKind::Claude, ProviderKind::Codex]
        .into_iter()
        .enumerate()
    {
        let turn = cold(provider.clone(), 5_340_663_000 + n as u64).await;
        let state = turn.cancel.herdr_interrupt_state().unwrap();
        {
            let mut input = state.submission.lock().unwrap();
            assert!(state.prepare_input(&mut input));
            state.finish_input(&mut input, HerdrSubmission::Submitted, false);
        }
        let (result, _) = turn.execute(turn.owner.clone()).await;
        let error = result.unwrap_err();
        let (_, holds) = turn.bridge(error.clone()).await;
        let case = format!("{provider:?}: {error}");
        let bodies = turn.shown.bodies();
        assert!(
            bodies.iter().any(|body| body.contains(&error)),
            "{case}: {bodies:?}"
        );
        assert!(!holds, "{case}");
        assert!(turn.durable().is_none(), "{case}");
        assert_eq!(turn.active(), 0, "{case}");
        let next = Arc::new(CancelToken::new());
        let message = MessageId::new(turn.row.user_msg_id + 1);
        let started = discord::mailbox_try_start_turn(
            &turn.shared,
            turn.channel,
            next,
            UserId::new(1),
            message,
        );
        assert!(started.await, "{case}: the next input starts");
    }
}

/// A turn the row pass already settled and woke leaves its bridge nothing to finish: no body is
/// shown and the row, the mailbox and the active count stay as that settlement left them.
#[tokio::test(flavor = "current_thread")]
async fn a_bridge_whose_turn_the_row_pass_settled_finishes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let turn = cold(ProviderKind::Claude, 5_340_664_000).await;
    let state = turn.cancel.herdr_interrupt_state().unwrap();
    {
        let mut input = state.submission.lock().unwrap();
        assert!(state.prepare_input(&mut input));
        state.finish_input(&mut input, HerdrSubmission::Submitted, false);
    }
    turn.cancel.mark_completion_cleanup();
    turn.cancel.cancelled.store(true, Ordering::Release);
    let (_, holds) = turn.bridge_frames(None).await;
    assert_eq!(turn.shown.bodies(), Vec::<String>::new());
    let row = turn.durable().map(|row| row.turn_nonce);
    assert_eq!(
        row,
        Some(turn.row.turn_nonce.clone()),
        "the row is the settlement's"
    );
    assert!(holds, "the mailbox is the settlement's");
    assert_eq!(turn.active(), 1, "the active turn is the settlement's");
}

#[path = "entry_abort_cold_error_tests/provider_failure_tests.rs"]
mod provider_failure_tests;
