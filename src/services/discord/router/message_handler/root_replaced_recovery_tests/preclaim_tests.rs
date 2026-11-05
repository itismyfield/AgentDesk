use super::*;
use crate::services::discord::queue_io::cancel_backstop_test_support as wakes;
use crate::services::discord::queue_io::input_owner::admission::{
    Admission, TurnBind, admit_with_context, exact_turn,
};
use crate::services::discord::queue_io::input_owner::drive::{Front, Handoff};
use crate::services::discord::queue_io::input_owner::ingress;
use crate::services::discord::queue_io::input_owner::origin::InputOrigin;
use crate::services::discord::router::message_handler::provider_dispatch::owner_input as owner;
use crate::services::herdr_launch::policy::{PermitFuture, UserDefaultCreateRequest};
use std::cell::RefCell;
use std::rc::Rc;
use tower::ServiceExt;

struct PolicyLauncher {
    io: Arc<Launcher>,
    policy: Arc<dyn HerdrLaunchHost>,
    source: Arc<dyn PeerSource>,
}

impl HerdrLaunchHost for PolicyLauncher {
    fn mode(&self, endpoint: &HerdrLaunchEndpoint) -> crate::config::session_hosts::EndpointMode {
        self.policy.mode(endpoint)
    }
    fn create_permit<'a>(&'a self, endpoint: &'a HerdrLaunchEndpoint) -> PermitFuture<'a> {
        self.policy.create_permit(endpoint)
    }
    fn current_witness(&self, endpoint: &HerdrLaunchEndpoint) -> Result<ServerWitness, String> {
        self.policy.current_witness(endpoint)
    }
    fn grants(&self) -> &dyn LaunchGrantStore {
        self.policy.grants()
    }
    fn remember_grant(
        &self,
        grant: crate::services::session_host::policy::permit::LaunchAdmissionGrant,
    ) {
        self.policy.remember_grant(grant);
    }
    fn create_user_default(&self, request: &UserDefaultCreateRequest) -> HerdrCreateOutcome {
        if let Err(error) = request
            .permit
            .fence(&request.endpoint, self.source.as_ref())
        {
            return HerdrCreateOutcome::NotSent(format!("{error:?}"));
        }
        self.io.create(&HerdrCreateRequest {
            endpoint: request.endpoint.clone(),
            label: request.label.clone(),
            cwd: request.cwd.clone(),
            command: request.command.clone(),
            restore_off_witness: request.permit.witness().clone(),
        })
    }
    fn restore_resume(&self, endpoint: &HerdrLaunchEndpoint) -> RestoreResume {
        self.io.restore_resume(endpoint)
    }
    fn create(&self, request: &HerdrCreateRequest) -> HerdrCreateOutcome {
        self.io.create(request)
    }
    fn launch_evidence(&self, probe: &EvidenceProbe) -> Result<ExpectedExecution, EvidenceGap> {
        self.io.launch_evidence(probe)
    }
}

struct ClaimedPorts<'a> {
    inner: Ports<'a>,
    host: Arc<dyn HerdrLaunchHost>,
}

impl HerdrTurnPorts for ClaimedPorts<'_> {
    fn launch_host(&self) -> Option<Arc<dyn HerdrLaunchHost>> {
        Some(self.host.clone())
    }
    fn hook_events(&self) -> tokio::sync::broadcast::Receiver<HookEvent> {
        self.inner.hook_events()
    }
    fn attach(&self, request: &AttachRequest<'_>) -> Result<bool, String> {
        self.inner.attach(request)
    }
    fn confirm_bound(
        &self,
        expected: &HostedOwner,
        record: &HostedExecution,
        target: &crate::services::session_host::HerdrTarget,
    ) -> Result<(), String> {
        self.inner.confirm_bound(expected, record, target)
    }
    fn earlier_tmux<'a>(&self, logical: &'a str) -> Box<dyn EarlierExecution + 'a> {
        self.inner.earlier_tmux(logical)
    }
    fn auth_overlay(&self, logical: &str) -> Result<ProviderAuthOverlay, String> {
        self.inner.auth_overlay(logical)
    }
}

/// Keeps the registered policy cache on this thread while the provider emits its real hook.
fn claimed_turn(
    case: &Case,
    text: &str,
    launch: crate::services::claude_tui::owner_input::LaunchHook,
    source: Option<Arc<dyn PeerSource>>,
) -> usize {
    let fx = &case.fx;
    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
    let endpoint = HerdrLaunchEndpoint {
        execution_node: endpoint.execution_node,
        config_key: endpoint.key,
        socket_addr: endpoint.socket_path.display().to_string(),
        herdr_session: endpoint.herdr_session,
    };
    let _registry = source.is_none().then(|| {
        fx.rig.registry_as_on_this_thread(
            &endpoint.execution_node,
            &endpoint.config_key,
            Path::new(&endpoint.socket_addr),
            &endpoint.herdr_session,
        )
    });
    let launcher = Arc::new(Launcher::default());
    let host: Arc<dyn HerdrLaunchHost> = match source {
        Some(source) => Arc::new(PolicyLauncher {
            io: launcher.clone(),
            policy: herdr_turn::boot_launch_host().unwrap(),
            source,
        }),
        None => launcher.clone(),
    };
    let ports = ClaimedPorts {
        inner: fx.ports(&launcher),
        host,
    };
    let row = fx.record();
    let app = crate::services::claude_tui::hook_server::hook_receiver_router_with_state(
        fx.ingress.state.clone(),
    );
    let (rig, finished, started) = (&fx.rig, &fx.finished, &fx.started);
    let (cwd, home) = (fx.cwd.path().to_path_buf(), fx.home());
    let log_root = crate::services::tui_prompt_dedupe::binding_events::test_root();
    let text = text.to_string();
    let _admission = open_admission();
    fx.finished.store(false, Ordering::SeqCst);
    let provider = std::thread::scope(|scope| {
        let launcher = launcher.clone();
        let prompt = text.clone();
        let player = scope.spawn(move || {
            crate::services::tui_prompt_dedupe::binding_events::set_test_root(log_root.as_deref());
            if !wait_for(finished, "the claimed launch evidence", || {
                !launcher.nonces.lock().unwrap().is_empty()
            }) {
                return;
            }
            let nonce = launcher.nonces.lock().unwrap()[0].clone();
            let context = context_of(&nonce);
            rig.exists(SHELL, true);
            rig.exists(PROVIDER, true);
            rig.foreground(&[PROVIDER]);
            rig.run_provider(&context, false);
            let launched = crate::services::tui_prompt_dedupe::binding_context::execution_context(
                "claude", &nonce,
            )
            .unwrap();
            let session = launched.expected_native_session_id.unwrap();
            let transcript = crate::services::claude_tui::transcript_tail::claude_transcript_path(
                &cwd,
                &session,
                home.as_deref(),
            )
            .unwrap();
            let envelope = HookBindingEnvelope::capture_from_env("claude", |name| {
                (name == "AGENTDESK_BINDING_CONTEXT").then(|| context.clone().into_os_string())
            })
            .encode()
            .unwrap();
            let payload = json!({"session_id":session,"source":"startup",
                "transcript_path":transcript});
            let request = axum::http::Request::post(format!(
                "/hooks/claude/SessionStart?session_id={session}"
            ))
            .header("content-type", "application/json")
            .header(
                crate::services::tui_prompt_dedupe::binding_context::BINDING_HEADER,
                envelope,
            )
            .body(axum::body::Body::from(payload.to_string()))
            .unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            started.store(true, Ordering::SeqCst);
            let response = runtime.block_on(app.oneshot(request)).unwrap();
            assert_eq!(response.status(), 202);
            if wait_for(finished, "the claimed prompt", || rig.sends().len() == 4) {
                append(
                    &transcript,
                    &[
                        json!({"type":"user","sessionId":session,
                            "message":{"role":"user","content":prompt}}),
                        json!({"type":"assistant","sessionId":session,
                            "message":{"role":"assistant","stop_reason":"end_turn",
                            "content":[{"type":"text","text":"recovered answer"}]}}),
                        json!({"type":"system","subtype":"turn_duration","sessionId":session}),
                    ],
                );
            }
        });
        let _launch = crate::services::claude_tui::owner_input::install_launch(launch);
        let (sender, receiver) = std::sync::mpsc::channel();
        let result = crate::services::tui_o::exact_submission::dispatch(|| {
            herdr_turn::execute(
                HerdrTurn {
                    pool: &fx.pool,
                    owner: fx.owner.clone(),
                    channel_id: CHANNEL,
                    endpoint,
                    row: Some(&row),
                    prompt: &text,
                    working_dir: fx.cwd.path().to_str().unwrap(),
                    session_id: None,
                    force_fresh: false,
                    system_prompt: None,
                    model: None,
                    hook_endpoint: Some("http://127.0.0.1:1".into()),
                    cancel: None,
                },
                &ports,
                sender,
            )
        });
        fx.finished.store(true, Ordering::SeqCst);
        let _: Vec<_> = receiver.try_iter().collect();
        player.join().unwrap();
        result
    });
    assert_eq!(provider, Ok(()));
    assert_eq!(ports.inner.attaches.lock().unwrap().len(), 1);
    assert_eq!(fx.rig.sends().len(), 4, "one prompt paste and one Enter");
    assert_eq!(
        fx.rig.sends()[2]["text"],
        format!("\u{1b}[200~{text}\u{1b}[201~")
    );
    launcher.creates.load(Ordering::SeqCst)
}

struct WakeCapture(ChannelId);

impl WakeCapture {
    fn install(channel: ChannelId) -> Self {
        assert!(wakes::set_owner_wake_for_tests(channel, Some(Arc::new(|| {}))).is_none());
        Self(channel)
    }
}

impl Drop for WakeCapture {
    fn drop(&mut self) {
        wakes::set_owner_wake_for_tests(self.0, None);
    }
}

enum Input {
    Discord,
    Routine,
    Scheduled,
    Http,
}

impl Input {
    fn context(&self) -> Option<Value> {
        match self {
            Self::Discord => None,
            Self::Routine => Some(json!({"origin":"routine"})),
            Self::Scheduled => Some(json!({"origin":"scheduled"})),
            Self::Http => Some(json!({"origin":"headless"})),
        }
    }
    fn origin(&self) -> InputOrigin {
        match self {
            Self::Discord => InputOrigin::Discord,
            Self::Routine | Self::Scheduled => InputOrigin::Routine,
            Self::Http => InputOrigin::HttpStart,
        }
    }
}

/// The live ingress stores Q; its first owner handoff reaches the real idle claim body.
fn owner_input<T: 'static>(
    fixture: T,
    case_of: fn(&T) -> &Case,
    source: Option<Arc<dyn PeerSource>>,
    input: Input,
) -> T {
    let case = case_of(&fixture);
    let (runtime, shared) = (case.fx.rt.handle().clone(), case.shared.clone());
    let effects_before = case.effects();
    let channel = ChannelId::new(CHANNEL);
    let _wake = WakeCapture::install(channel);
    let wakes_before = wakes::owner_wakes(channel);
    let fixture = Rc::new(RefCell::new(Some(fixture)));
    let queued = queued_input();
    let original = ingress::original(&ProviderKind::Claude, channel, None, &queued).unwrap();
    let (expected, ticket) = (original.item.clone(), original.ticket.clone());
    let observed = fixture.clone();
    let creates = Arc::new(AtomicUsize::new(0));
    let seen_creates = creates.clone();
    let callback_expected = expected.clone();
    let callback_ticket = ticket.clone();
    let wanted = input.origin();
    let _launch = owner::install_root_recovery_launch(
        channel,
        Box::new(move |claim, text| {
            tokio::task::block_in_place(|| {
                assert_eq!(claim.front().0.as_deref(), Some(callback_ticket.as_str()));
                assert_eq!(claim.front().1.message_id, callback_expected.message_id);
                assert_eq!(text, callback_expected.text);
                let owned = observed.borrow();
                let case = case_of(owned.as_ref().unwrap());
                let prelaunch_state = case.fx.row();
                assert_eq!(
                    case.effects(),
                    effects_before,
                    "preclaim recovery writes or closes no old pane"
                );
                let launch = owner::claimed_root_recovery_launch(claim, text, wanted);
                let count = claimed_turn(case, text, launch, source);
                assert_eq!(
                    prelaunch_state,
                    Some(HostedState::Retired),
                    "the real idle path recovered before claiming and launching the same Q"
                );
                seen_creates.store(count, Ordering::SeqCst);
                Ok(())
            })
        }),
    );
    runtime.block_on(ingress::ADOPTS.scope(true, async {
        let (_, _http) =
            crate::services::discord::shared_state::test_rest::recording_mock(900, CHANNEL).await;
        shared
            .http
            .cached_bot_token
            .set("root-preclaim-test".into())
            .unwrap();
        let mailbox = shared.mailbox(channel);
        assert_eq!(
            ingress::adopt(&shared, &ProviderKind::Claude, channel).await,
            Ok(true)
        );
        match input.context() {
            None => {
                let outcome = ingress::enqueue(&shared, &ProviderKind::Claude, channel, &queued)
                    .await
                    .unwrap();
                assert!(outcome.enqueued, "{outcome:?}");
                assert!(outcome.persistence_error.is_none());
                assert_eq!(wakes::owner_wakes(channel), wakes_before + 1);
            }
            Some(context) => assert!(matches!(
                admit_with_context(&mailbox, &expected, ticket.clone(), context).await,
                Admission::Accepted(_)
            )),
        }
        let front = mailbox
            .v2_request(|slot| {
                let owner = slot.as_mut().ok_or("owner not installed")?;
                let front = owner.front().ok_or("original absent")?;
                owner.grant_front(&front)?;
                Ok(front)
            })
            .await
            .unwrap();
        let at = shared.clone();
        let idle: Handoff =
            Arc::new(move |front| Box::pin(owner::start_idle_for_test(at.clone(), channel, front)));
        let drive = owner::owner_drive(
            (
                mailbox.clone(),
                Some(shared.pg_pool.as_ref().unwrap().clone()),
            ),
            (ProviderKind::Claude, CHANNEL),
            idle,
            Some(shared.clone()),
        );
        let wake_count = wakes::owner_wakes(channel);
        assert_eq!((drive.handoff)(front).await, Ok(()));
        assert_eq!(creates.load(Ordering::SeqCst), 1);
        assert_eq!(
            wakes::owner_wakes(channel),
            wake_count,
            "no recovery wake is needed"
        );
        let state = mailbox
            .v2_request(|slot| slot.as_ref().ok_or("owner absent")?.json())
            .await
            .unwrap();
        assert_eq!(state["items"], json!([]));
        assert!(state["head_injection"].is_null());
        assert_eq!(state["retained_inputs"], json!([]));
        assert!(matches!(
            exact_turn(&mailbox, &ticket, &expected).await,
            Ok(TurnBind::Settled(_))
        ));
        assert!(
            crate::services::discord::inflight::load_inflight_state_read_only_result(
                &ProviderKind::Claude,
                CHANNEL,
            )
            .unwrap()
            .is_none()
        );
    }));
    drop(_launch);
    Rc::try_unwrap(fixture).ok().unwrap().into_inner().unwrap()
}

#[test]
fn rr_preclaim_retired_without_location_runs_the_ingress_original_once_pg() {
    let case = Case::launched("rr-preclaim-retired-none");
    let _intake = IntakeGate::configure(&case);
    let HostedRecord::Known(mut retired) = case.fx.record() else {
        unreachable!()
    };
    retired.state = HostedState::Retired;
    retired.location = None;
    case.fx.store(&retired);
    let case = owner_input(case, |case| case, None, Input::Discord);
    assert_eq!(case.fx.row(), Some(HostedState::Bound));
    assert_eq!(
        case.cleanups(),
        0,
        "Retired goes through its existing fresh launch cleanup"
    );
}

#[test]
fn rr_preclaim_retired_from_another_endpoint_runs_the_ingress_original_once_pg() {
    let case = Case::launched("rr-preclaim-retired-moved");
    let _intake = IntakeGate::configure(&case);
    let HostedRecord::Known(mut retired) = case.fx.record() else {
        unreachable!()
    };
    retired.state = HostedState::Retired;
    retired.location.as_mut().unwrap().endpoint_config_key = "prior-endpoint".into();
    case.fx.store(&retired);
    let case = owner_input(case, |case| case, None, Input::Discord);
    assert_eq!(case.fx.row(), Some(HostedState::Bound));
    assert_eq!(case.cleanups(), 0);
}

#[test]
fn rr_preclaim_first_ingress_before_reader_recovers_and_submits_the_same_q_pg() {
    let case = PolicyCase::new("rr-preclaim-race", Some(8), false, false, false);
    assert!(case.evidence().startup.is_none());
    assert_eq!(ATTACHES.with(std::cell::Cell::get), 0);
    let runtime = case.case.fx.rt.handle().clone();
    let _entered = runtime.enter();
    let channel = ChannelId::new(CHANNEL);
    let mailbox = case.case.shared.mailbox(channel);
    let original =
        ingress::original(&ProviderKind::Claude, channel, None, &queued_input()).unwrap();
    let expected_ticket = original.ticket.as_str().to_string();
    let expected = original.item;
    let policy = herdr_endpoints().launch_host().unwrap();
    let endpoint = case.endpoint.clone();
    let nonce = case.case.nonce.clone();
    let before_cas_runs = Arc::new(AtomicUsize::new(0));
    let observed_cas = before_cas_runs.clone();
    recovery::BEFORE_CAS.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            Box::pin(async move {
                observed_cas.fetch_add(1, Ordering::SeqCst);
                assert!(
                    policy
                        .mutation_evidence(&endpoint, &nonce)
                        .unwrap()
                        .startup
                        .is_some(),
                    "the preclaim producer made startup proof before retirement or the fresh create"
                );
                let (front, claimed, claim_number): (Front, bool, u64) = mailbox
                    .v2_request(|slot| {
                        let owner = slot.as_ref().ok_or("owner absent at recovery CAS")?;
                        let front = owner.front().ok_or("original absent at recovery CAS")?;
                        let claimed = owner.claimed(front.1.message_id, &front);
                        Ok((front, claimed, owner.claim_number()))
                    })
                    .await
                    .unwrap();
                assert_eq!(front.0.as_deref(), Some(expected_ticket.as_str()));
                assert_eq!(front.1.message_id, expected.message_id);
                assert_eq!(front.1.author_id, expected.author_id);
                assert_eq!(front.1.queued_generation, expected.queued_generation);
                assert_eq!(front.1.source_message_ids, expected.source_message_ids);
                assert_eq!(front.1.text, expected.text);
                assert!(
                    !claimed,
                    "root recovery completes before the owner turn claim"
                );
                assert_eq!(
                    claim_number, 0,
                    "root recovery completes before the owner turn claim"
                );
            })
        }));
    });
    let source: Arc<dyn PeerSource> = case.source.clone();
    let case = owner_input(case, |case| &case.case, Some(source), Input::Discord);
    assert_eq!(
        before_cas_runs.load(Ordering::SeqCst),
        1,
        "the live owner recovery checked actor custody at BEFORE_CAS exactly once"
    );
    assert_eq!(case.case.cleanups(), 1);
    assert!(case.evidence().startup.is_some());
    assert_eq!(
        case.evidence().grant.map(|grant| grant.encode().unwrap()),
        case.original_grant
    );
    let HostedRecord::Known(record) = case.case.fx.record() else {
        unreachable!()
    };
    assert_eq!(record.state, HostedState::Bound);
    assert_ne!(record.execution_nonce, case.case.nonce);
}

#[test]
fn rr_preclaim_routine_scheduled_and_http_keep_their_original_and_provenance_pg() {
    for (label, input) in [
        ("routine", Input::Routine),
        ("scheduled", Input::Scheduled),
        ("http", Input::Http),
    ] {
        let case = PolicyCase::new(
            &format!("rr-preclaim-{label}"),
            Some(8),
            false,
            false,
            false,
        );
        let source: Arc<dyn PeerSource> = case.source.clone();
        let case = owner_input(case, |case| &case.case, Some(source), input);
        assert_eq!(case.case.cleanups(), 1);
        assert!(case.evidence().startup.is_some());
    }
}

fn pane_reads(case: &Case) -> usize {
    case.fx
        .rig
        .requests()
        .iter()
        .filter(|request| {
            matches!(
                request["method"].as_str(),
                Some("pane.get" | "pane.read" | "pane.process_info")
            )
        })
        .count()
}

#[test]
fn rr_preclaim_healthy_bound_without_replacement_evidence_reads_no_pane_pg() {
    let case = PolicyCase::new("rr-preclaim-healthy", Some(7), false, false, false);
    case.case
        .fx
        .rig
        .run_provider(&context_of(&case.case.nonce), false);
    case.case.fx.rig.exists(SHELL, true);
    case.case.fx.rig.exists(PROVIDER, true);
    case.case.fx.rig.foreground(&[PROVIDER]);
    std::fs::write(&case.source.peer.config, "[invalid config").unwrap();
    let before = pane_reads(&case.case);
    let result = case.case.fx.rt.block_on(recovery::preclaim::before_idle(
        &case.case.shared,
        &ProviderKind::Claude,
        ChannelId::new(CHANNEL),
    ));
    assert!(result.is_ok());
    assert_eq!(
        pane_reads(&case.case),
        before,
        "healthy idle inputs do not inspect the pane"
    );
    assert_eq!(case.case.cleanups(), 0);
    assert_eq!(case.checker.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn rr_preclaim_pending_and_nonlocal_bound_rows_are_untouched_pg() {
    let case = PolicyCase::new("rr-preclaim-unrelated", Some(8), false, false, false);
    let HostedRecord::Known(original) = case.case.fx.record() else {
        unreachable!()
    };
    for change in ["pending", "node", "endpoint", "unplaced"] {
        let mut record = original.clone();
        match change {
            "pending" => record.state = HostedState::Pending,
            "node" => record.owner.owner_node = "another-node".into(),
            "endpoint" => {
                record.location.as_mut().unwrap().endpoint_config_key = "another-endpoint".into()
            }
            _ => record.location = None,
        }
        case.case.fx.store(&record);
        let before = case.case.fx.record();
        let requests = case.case.fx.rig.requests().len();
        assert!(
            case.case
                .fx
                .rt
                .block_on(recovery::preclaim::before_idle(
                    &case.case.shared,
                    &ProviderKind::Claude,
                    ChannelId::new(CHANNEL),
                ))
                .is_ok(),
            "{change}"
        );
        assert_eq!(case.case.fx.record(), before, "{change}");
        assert_eq!(
            case.case.fx.rig.requests().len(),
            requests,
            "{change}: no peer I/O"
        );
        assert_eq!(case.case.cleanups(), 0);
    }
}

#[test]
fn rr_preclaim_exact_live_read_precedes_a_cached_replaced_candidate_pg() {
    let case = PolicyCase::new("rr-preclaim-exact-live", Some(8), false, false, false);
    case.case.fx.rig.exists(PROVIDER, true);
    case.pass();
    assert_eq!(case.case.fx.row(), Some(HostedState::Bound));
    assert!(case.evidence().startup.is_some());
    case.case
        .fx
        .rig
        .run_provider(&context_of(&case.case.nonce), false);
    case.case.fx.rig.exists(SHELL, true);
    case.case.fx.rig.foreground(&[PROVIDER]);
    let row = case
        .case
        .fx
        .rt
        .block_on(
            crate::services::discord::recovery_engine::herdr_server_replaced::canonical_row(
                &case.case.fx.pool,
                TOKEN,
                &ProviderKind::Claude,
                ChannelId::new(CHANNEL),
            ),
        )
        .unwrap();
    assert!(matches!(
        crate::services::discord::recovery_engine::herdr_reader::prepare_root_replaced_candidate(&row)
            .unwrap().observe(),
        Ok(None)
    ), "the fixture now reads the original root/provider as an exact match");
    let before = case.case.effects();
    let result = case.case.fx.rt.block_on(recovery::preclaim::before_idle(
        &case.case.shared,
        &ProviderKind::Claude,
        ChannelId::new(CHANNEL),
    ));
    assert_eq!(result, Ok(true));
    assert_eq!(case.case.fx.row(), Some(HostedState::Bound));
    assert_eq!(case.case.cleanups(), 0);
    assert_eq!(
        case.case.effects(),
        before,
        "a fresh exact root remains alive"
    );
}

#[test]
fn rr_preclaim_policy_held_keeps_the_unclaimed_front_and_owes_no_screen_notice_pg() {
    let case = PolicyCase::new("rr-preclaim-held", Some(8), true, false, false);
    let shared = case.case.shared.clone();
    let channel = ChannelId::new(CHANNEL);
    let _wake = WakeCapture::install(channel);
    let _launch = owner::install_root_recovery_launch(
        channel,
        Box::new(|claim, _| {
            claim.defer_before_submit("unexpected root recovery launch");
            Err("unexpected root recovery launch".into())
        }),
    );
    let before = case.case.effects();
    case.case.fx.rt.block_on(ingress::ADOPTS.scope(true, async {
        let (requests, _http) =
            crate::services::discord::shared_state::test_rest::recording_mock(950, CHANNEL).await;
        shared
            .http
            .cached_bot_token
            .set("root-preclaim-test".into())
            .unwrap();
        let queued = queued_input();
        assert!(
            ingress::enqueue(&shared, &ProviderKind::Claude, channel, &queued)
                .await
                .unwrap()
                .enqueued
        );
        let mailbox = shared.mailbox(channel);
        let front = mailbox
            .v2_request(|slot| {
                let owner = slot.as_mut().ok_or("owner absent")?;
                let front = owner.front().ok_or("original absent")?;
                owner.grant_front(&front)?;
                Ok(front)
            })
            .await
            .unwrap();
        let kept = front.clone();
        let starts = shared.clone();
        let idle: Handoff = Arc::new(move |front| {
            Box::pin(owner::start_idle_for_test(starts.clone(), channel, front))
        });
        let drive = owner::owner_drive(
            (mailbox.clone(), None),
            (ProviderKind::Claude, CHANNEL),
            idle,
            Some(shared.clone()),
        );
        let refused = (drive.handoff)(front).await;
        assert_eq!(refused, Err("NotSent(\"herdr_root_recovery_held\")".into()));
        assert_eq!(
            ingress::Hold::from_refusal(refused.as_ref().unwrap_err()),
            None
        );
        assert!(
            requests.lock().unwrap().is_empty(),
            "the fixed code owes no TUI/screen notice"
        );
        let (claimed, snapshot) = mailbox
            .v2_request(move |slot| {
                let owner = slot.as_ref().ok_or("owner absent")?;
                Ok((owner.claimed(kept.1.message_id, &kept), owner.json()?))
            })
            .await
            .unwrap();
        assert!(!claimed);
        assert_eq!(snapshot["items"].as_array().unwrap().len(), 1);
        assert!(snapshot["head_injection"].is_null());
    }));
    assert_eq!(case.case.fx.row(), Some(HostedState::Bound));
    assert_eq!(case.case.cleanups(), 0);
    assert_eq!(case.case.effects(), before);
    assert!(case.evidence().startup.is_none());
}

#[test]
fn rr_preclaim_legacy_queue_recovers_before_taking_the_original_pg() {
    let case = PolicyCase::new("rr-preclaim-legacy", Some(8), false, false, false);
    let runtime = case.case.fx.rt.handle().clone();
    let _entered = runtime.enter();
    let channel = ChannelId::new(CHANNEL);
    let queued = queued_input();
    case.case
        .fx
        .rt
        .block_on(case.case.shared.mailbox(channel).replace_queue(
            vec![queued.clone()],
            crate::services::discord::queue_persistence_context(
                &case.case.shared,
                &ProviderKind::Claude,
                channel,
            ),
        ));
    assert_eq!(
        case.case
            .fx
            .rt
            .block_on(ingress::installed(&case.case.shared.mailbox(channel),)),
        Ok(false)
    );
    let shared = case.case.shared.clone();
    let expected = queued.message_id;
    let checked = Arc::new(AtomicBool::new(false));
    let before_take = checked.clone();
    recovery::BEFORE_CAS.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            Box::pin(async move {
                let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel).await;
                assert!(
                    snapshot.pending_user_dispatch.is_none(),
                    "recovery precedes dequeue"
                );
                assert_eq!(snapshot.intervention_queue.len(), 1);
                assert_eq!(snapshot.intervention_queue[0].message_id, expected);
                before_take.store(true, Ordering::SeqCst);
            })
        }));
    });
    let before = case.case.effects();
    let taken = case.case.fx.rt.block_on(
        crate::services::discord::mailbox_take_next_automatic_intervention(
            &case.case.shared,
            &ProviderKind::Claude,
            channel,
        ),
    );
    assert!(
        checked.load(Ordering::SeqCst),
        "the production pre-dequeue path recovered N1"
    );
    assert_eq!(case.case.fx.row(), Some(HostedState::Retired));
    assert_eq!(case.case.cleanups(), 1);
    assert!(taken.persistence_error.is_none());
    assert_eq!(
        taken.intervention.as_ref().map(|item| item.message_id),
        Some(queued.message_id)
    );
    assert_eq!(
        case.case.effects(),
        before,
        "root recovery creates/writes/closes nothing"
    );
    assert!(case.evidence().startup.is_some());
}

#[test]
fn rr_preclaim_legacy_queue_held_preserves_the_original_before_any_take_pg() {
    let case = PolicyCase::new("rr-preclaim-legacy-held", Some(8), true, false, false);
    let runtime = case.case.fx.rt.handle().clone();
    let _entered = runtime.enter();
    let channel = ChannelId::new(CHANNEL);
    let queued = queued_input();
    case.case
        .fx
        .rt
        .block_on(case.case.shared.mailbox(channel).replace_queue(
            vec![queued.clone()],
            crate::services::discord::queue_persistence_context(
                &case.case.shared,
                &ProviderKind::Claude,
                channel,
            ),
        ));
    let before = case.case.effects();
    let taken = case.case.fx.rt.block_on(
        crate::services::discord::mailbox_take_next_automatic_intervention(
            &case.case.shared,
            &ProviderKind::Claude,
            channel,
        ),
    );
    assert!(taken.intervention.is_none());
    assert!(taken.dispatch_lease.is_none());
    let snapshot = case
        .case
        .fx
        .rt
        .block_on(crate::services::discord::mailbox_snapshot(
            &case.case.shared,
            channel,
        ));
    assert!(snapshot.pending_user_dispatch.is_none());
    assert_eq!(snapshot.intervention_queue.len(), 1);
    assert_eq!(snapshot.intervention_queue[0].message_id, queued.message_id);
    assert_eq!(snapshot.intervention_queue[0].text, queued.text);
    assert_eq!(case.case.fx.row(), Some(HostedState::Bound));
    assert_eq!(case.case.cleanups(), 0);
    assert_eq!(case.case.effects(), before);
    assert!(case.evidence().startup.is_none());
    assert!(
        matches!(case.result(), recovery::Recovery::Held(ref why)
        if why.contains("PolicyUnverified(Identity)")),
        "the pre-dequeue path itself recorded the policy hold"
    );
}
