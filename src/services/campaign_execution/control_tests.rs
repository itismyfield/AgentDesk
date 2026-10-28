use super::*;
use serde_json::json;

pub(crate) fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-28T06:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

pub(crate) fn input() -> DefinitionInput {
    serde_json::from_value(json!({
        "repository": "adk", "title": "Campaign", "goal": "Verified work",
        "profile_version": "v1", "nodes": [
            {"id": "a", "title": "A", "issue_url": "https://github.com/itismyfield/AgentDesk/issues/7438",
             "acceptance": [{"id": "ac-a", "description": "Prove A", "verification": "test"}], "dependencies": []},
            {"id": "b", "title": "B", "issue_url": "https://github.com/itismyfield/AgentDesk/issues/7438",
             "acceptance": [{"id": "ac-b", "description": "Prove B", "verification": "test"}], "dependencies": ["a"]},
            {"id": "c", "title": "C", "issue_url": "https://github.com/itismyfield/AgentDesk/issues/7438",
             "acceptance": [{"id": "ac-c", "description": "Prove C", "verification": "operational"}], "dependencies": []}
        ]
    })).unwrap()
}

pub(crate) fn envelope(command: Command) -> CommandEnvelope {
    CommandEnvelope {
        idempotency_key: "key".into(),
        observed_at: now(),
        command,
    }
}

pub(crate) fn prepared() -> CampaignState {
    create_definition("campaign".into(), input(), "create-key", now())
        .unwrap()
        .state
}

pub(crate) fn running() -> CampaignState {
    let mut state = prepared();
    state.control = CampaignControl::Running;
    let gate = HostGate {
        host: "remote".into(),
        gate: "launch".into(),
        incarnation: "boot-2".into(),
    };
    let binding = AttemptBinding {
        campaign_id: state.id.clone(),
        run_id: "run".into(),
        node_id: "a".into(),
        series: 1,
        stage: Stage::Implementation,
        attempt_id: "attempt".into(),
        role: Role::Implementation,
        session: SessionIdentity {
            gate,
            session: "worker".into(),
            incarnation: "process-7".into(),
        },
        repository: Repository::Adk,
        hashes: state.definition.hashes()["a"].clone(),
        workflow_version: WORKFLOW_VERSION.into(),
        base_sha: "base".into(),
        candidate_sha: "candidate".into(),
        control_version: 1,
    };
    state.nodes.insert(
        "a".into(),
        NodeRuntime {
            stage: Stage::Implementation,
            disposition: NodeDisposition::Running,
            current_attempt: Some(binding.attempt_id.clone()),
            ..NodeRuntime::default()
        },
    );
    state.run = Some(Run {
        id: "run".into(),
        control_version: 1,
        stop: None,
        attempts: [(
            binding.attempt_id.clone(),
            Attempt {
                binding,
                started: true,
                outcome: None,
            },
        )]
        .into_iter()
        .collect(),
        actions: BTreeMap::new(),
        sessions: Vec::new(),
    });
    state
}

pub(crate) fn binding(state: &CampaignState) -> AttemptBinding {
    state.run.as_ref().unwrap().attempts["attempt"]
        .binding
        .clone()
}

pub(crate) fn with_action(phase: ActionState, started: bool) -> CampaignState {
    let mut state = running();
    let binding = binding(&state);
    let run = state.run.as_mut().unwrap();
    run.attempts.get_mut("attempt").unwrap().started = started;
    if phase == ActionState::Started {
        run.sessions.push(SessionRecord {
            identity: binding.session.clone(),
            attempt_id: binding.attempt_id.clone(),
            observation: SessionObservation::Gone {
                observed_at: now(),
                evidence: "reachable host: exact process absent".into(),
            },
        });
    }
    run.actions.insert(
        "action".into(),
        Action {
            id: "action".into(),
            target: binding.session.gate.clone(),
            binding,
            kind: ActionKind::Launch,
            state: phase,
            initiation_receipt: (phase == ActionState::Started).then(|| "effect-1".into()),
            reconciliation: None,
        },
    );
    state
}

pub(crate) fn step(state: &CampaignState, command: Command) -> CampaignState {
    let mut command = envelope(command);
    command.idempotency_key = format!("key-{}", state.event_sequence);
    decide(state, &command).unwrap().state
}

pub(crate) fn stop(state: &CampaignState, mode: StopMode) -> CampaignState {
    step(
        state,
        Command::RequestStop {
            expected_revision: state.definition_revision,
            expected_run: state.run.as_ref().unwrap().id.clone(),
            mode,
        },
    )
}

fn error(state: &CampaignState, command: Command, expected: RejectionCode) {
    assert_eq!(
        decide(state, &envelope(command)).unwrap_err().code,
        expected
    );
}

fn observe(state: &CampaignState, observation: ActionObservation) -> CampaignState {
    step(
        state,
        Command::ObserveAction {
            action_id: "action".into(),
            binding: Box::new(binding(state)),
            observation,
        },
    )
}

fn acknowledge(state: &CampaignState) -> CampaignState {
    let run = state.run.as_ref().unwrap();
    let target = run.actions["action"].target.clone();
    step(
        state,
        Command::AcknowledgeFence {
            target,
            control_version: run.control_version,
        },
    )
}

#[test]
fn restored_action_without_sessions_requires_exact_fence_and_initiation_classification() {
    let mut original = with_action(ActionState::StartPending, false);
    original
        .run
        .as_mut()
        .unwrap()
        .actions
        .get_mut("action")
        .unwrap()
        .target
        .gate = "effect-gate".into();
    let restored: CampaignState =
        serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
    let requested = stop(&restored, StopMode::FinishStage);
    let run = requested.run.as_ref().unwrap();
    assert!(run.sessions.is_empty());
    assert_eq!(run.control_version, 2);
    assert_eq!(run.attempts["attempt"].binding.control_version, 1);
    assert!(
        run.stop
            .as_ref()
            .unwrap()
            .targets
            .contains(&run.actions["action"].target)
    );
    assert!(
        run.stop
            .as_ref()
            .unwrap()
            .targets
            .contains(&binding(&requested).session.gate)
    );
    assert!(run.stop.as_ref().unwrap().confirmed_event.is_none());
    error(
        &requested,
        Command::ConfirmStopFence,
        RejectionCode::FenceIncomplete,
    );
    error(
        &requested,
        Command::AcknowledgeFence {
            target: run.actions["action"].target.clone(),
            control_version: 2,
        },
        RejectionCode::UnreconciledActions,
    );
    let mut ack_only = requested.clone();
    ack_only
        .run
        .as_mut()
        .unwrap()
        .stop
        .as_mut()
        .unwrap()
        .acknowledgements
        .push(FenceAcknowledgement {
            target: run.actions["action"].target.clone(),
            control_version: 2,
        });
    error(
        &ack_only,
        Command::ConfirmStopFence,
        RejectionCode::UnreconciledActions,
    );
    let classified = observe(
        &requested,
        ActionObservation::NotStarted {
            evidence: "gate journal: absent".into(),
        },
    );
    assert_eq!(
        classified.run.as_ref().unwrap().attempts["attempt"].outcome,
        Some(AttemptOutcome::UserInterrupted)
    );
    error(
        &classified,
        Command::ConfirmStopped,
        RejectionCode::FenceIncomplete,
    );
    let acknowledged = acknowledge(&classified);
    error(
        &acknowledged,
        Command::ConfirmStopFence,
        RejectionCode::FenceIncomplete,
    );
    let acknowledged = step(
        &acknowledged,
        Command::AcknowledgeFence {
            target: binding(&classified).session.gate,
            control_version: 2,
        },
    );
    let fenced = step(&acknowledged, Command::ConfirmStopFence);
    assert_eq!(fenced.control, CampaignControl::Stopping);
    assert!(
        fenced
            .run
            .as_ref()
            .unwrap()
            .stop
            .as_ref()
            .unwrap()
            .confirmed_event
            .is_some()
    );
    let stopped = step(&fenced, Command::ConfirmStopped);
    assert_eq!(stopped.control, CampaignControl::Stopped);
    assert!(edit_guard(&stopped).is_ok());
}

#[test]
fn stop_cancels_unsent_intents_without_quality_failure() {
    for phase in [ActionState::Planned, ActionState::Claimed] {
        for mode in [StopMode::Immediate, StopMode::FinishStage] {
            let state = stop(&with_action(phase, false), mode);
            let run = state.run.as_ref().unwrap();
            assert_eq!(run.actions["action"].state, ActionState::Cancelled);
            assert_eq!(
                run.attempts["attempt"].outcome,
                Some(AttemptOutcome::UserInterrupted)
            );
            assert!(!run.attempts["attempt"].started);
            assert_eq!(state.nodes["a"].quality_failures, 0);
        }
    }
}

#[test]
fn finish_stage_preserves_v1_binding_but_forbids_v1_new_start_and_old_attempt_results() {
    let original = running();
    let pin = binding(&original);
    let stopped = stop(&original, StopMode::FinishStage);
    assert_eq!(validate_result_binding(&stopped, &pin), Ok(()));
    assert_eq!(binding(&stopped), pin);
    assert_eq!(stopped.run.as_ref().unwrap().control_version, 2);
    assert_eq!(
        validate_new_start(&stopped, &pin).unwrap_err().code,
        RejectionCode::InvalidState
    );
    let mut resumed = stopped.clone();
    resumed.control = CampaignControl::Running;
    assert_eq!(
        validate_new_start(&resumed, &pin).unwrap_err().code,
        RejectionCode::StaleFence
    );
    let action = with_action(ActionState::Planned, true)
        .run
        .unwrap()
        .actions
        .remove("action")
        .unwrap();
    error(
        &resumed,
        Command::PlanAction {
            action: Box::new(action),
        },
        RejectionCode::StaleFence,
    );
    let mut newer = stopped.clone();
    newer.nodes.get_mut("a").unwrap().current_attempt = Some("next-attempt".into());
    assert_eq!(
        validate_result_binding(&newer, &pin).unwrap_err().code,
        RejectionCode::StaleBinding
    );
    let mut rewritten = pin.clone();
    rewritten.control_version = 2;
    assert_eq!(
        validate_result_binding(&stopped, &rewritten)
            .unwrap_err()
            .code,
        RejectionCode::StaleBinding
    );
    let mut different_series = stopped.clone();
    different_series.nodes.get_mut("a").unwrap().series += 1;
    assert_eq!(
        validate_result_binding(&different_series, &pin)
            .unwrap_err()
            .code,
        RejectionCode::StaleBinding
    );
}

#[test]
fn finish_stage_accepts_late_pre_fence_initiation_without_reopening_new_actions() {
    let requested = stop(
        &with_action(ActionState::StartPending, false),
        StopMode::FinishStage,
    );
    assert!(validate_result_binding(&requested, &binding(&requested)).is_err());
    let started = observe(
        &requested,
        ActionObservation::Started {
            receipt: "effect-1".into(),
        },
    );
    assert_eq!(
        validate_result_binding(&started, &binding(&started)),
        Ok(())
    );
    let action = started.run.as_ref().unwrap().actions["action"].clone();
    error(
        &started,
        Command::PlanAction {
            action: Box::new(action),
        },
        RejectionCode::InvalidState,
    );
    let fenced = step(&acknowledge(&started), Command::ConfirmStopFence);
    error(
        &fenced,
        Command::ConfirmStopped,
        RejectionCode::SessionsNotGone,
    );
    let reconciled = observe(
        &fenced,
        ActionObservation::Reconciled {
            evidence: "exit and external identity".into(),
        },
    );
    error(
        &reconciled,
        Command::ConfirmStopped,
        RejectionCode::SessionsNotGone,
    );
    let mut reconciled = step(
        &reconciled,
        Command::ObserveSession {
            identity: binding(&reconciled).session,
            observation: SessionObservation::Gone {
                observed_at: now(),
                evidence: "reachable host: exact process absent".into(),
            },
        },
    );
    error(
        &reconciled,
        Command::ConfirmStopped,
        RejectionCode::InvalidState,
    );
    // F3 owns evidence acceptance; this fixture stores a result only after the binding gate.
    validate_result_binding(&reconciled, &binding(&reconciled)).unwrap();
    reconciled
        .run
        .as_mut()
        .unwrap()
        .attempts
        .get_mut("attempt")
        .unwrap()
        .outcome = Some(AttemptOutcome::Passed);
    assert_eq!(
        step(&reconciled, Command::ConfirmStopped).control,
        CampaignControl::Stopped
    );
}

#[test]
fn escalation_requires_new_fence_and_interrupts_without_erasing_facts_or_budget() {
    let mut original = with_action(ActionState::Started, true);
    original.nodes.get_mut("a").unwrap().quality_failures = 2;
    original.nodes.get_mut("a").unwrap().main_integrated = true;
    let finish = step(
        &acknowledge(&stop(&original, StopMode::FinishStage)),
        Command::ConfirmStopFence,
    );
    let immediate = stop(&finish, StopMode::Immediate);
    let run = immediate.run.as_ref().unwrap();
    assert_eq!(run.control_version, 3);
    assert_eq!(run.attempts["attempt"].binding.control_version, 1);
    assert_eq!(
        run.attempts["attempt"].outcome,
        Some(AttemptOutcome::UserInterrupted)
    );
    assert!(run.stop.as_ref().unwrap().acknowledgements.is_empty());
    assert!(run.stop.as_ref().unwrap().confirmed_event.is_none());
    assert!(validate_result_binding(&immediate, &binding(&immediate)).is_err());
    assert_eq!(immediate.nodes["a"].quality_failures, 2);
    assert!(immediate.nodes["a"].main_integrated);
    let weaker = stop(&immediate, StopMode::FinishStage);
    assert_eq!(weaker, immediate);
    error(
        &immediate,
        Command::ConfirmStopFence,
        RejectionCode::FenceIncomplete,
    );
    let reconciled = observe(
        &immediate,
        ActionObservation::Reconciled {
            evidence: "actual effect persists".into(),
        },
    );
    let fenced = step(&acknowledge(&reconciled), Command::ConfirmStopFence);
    assert_eq!(
        step(&fenced, Command::ConfirmStopped).control,
        CampaignControl::Stopped
    );
}

#[test]
fn both_stop_modes_allow_late_external_facts_even_from_a_prior_attempt() {
    for mode in [StopMode::Immediate, StopMode::FinishStage] {
        let mut state = with_action(ActionState::StartPending, false);
        state.nodes.get_mut("a").unwrap().current_attempt = Some("new-attempt".into());
        let requested = stop(&state, mode);
        let started = observe(
            &requested,
            ActionObservation::Started {
                receipt: "old-effect".into(),
            },
        );
        assert!(validate_result_binding(&started, &binding(&started)).is_err());
        let reconciled = observe(
            &started,
            ActionObservation::Reconciled {
                evidence: "actual PR identity".into(),
            },
        );
        assert_eq!(
            reconciled.run.as_ref().unwrap().actions["action"].state,
            ActionState::Reconciled
        );
        assert!(validate_new_start(&reconciled, &binding(&reconciled)).is_err());
    }
}

#[test]
fn immediate_late_start_never_becomes_partial_pass() {
    let requested = stop(
        &with_action(ActionState::StartPending, false),
        StopMode::Immediate,
    );
    let started = observe(
        &requested,
        ActionObservation::Started {
            receipt: "effect-before-fence".into(),
        },
    );
    assert!(started.run.as_ref().unwrap().attempts["attempt"].started);
    assert_eq!(
        started.run.as_ref().unwrap().attempts["attempt"].outcome,
        Some(AttemptOutcome::UserInterrupted)
    );
    assert!(validate_result_binding(&started, &binding(&started)).is_err());
    let reconciled = observe(
        &started,
        ActionObservation::Reconciled {
            evidence: "external effect only".into(),
        },
    );
    assert!(validate_result_binding(&reconciled, &binding(&reconciled)).is_err());
}

#[test]
fn fence_ack_requires_exact_gate_incarnation_and_current_version_after_restore() {
    let requested = stop(
        &with_action(ActionState::Started, true),
        StopMode::Immediate,
    );
    let target = requested.run.as_ref().unwrap().actions["action"]
        .target
        .clone();
    let mut old_gate = target.clone();
    old_gate.incarnation = "boot-1".into();
    error(
        &requested,
        Command::AcknowledgeFence {
            target: old_gate.clone(),
            control_version: 2,
        },
        RejectionCode::StaleBinding,
    );
    error(
        &requested,
        Command::AcknowledgeFence {
            target: target.clone(),
            control_version: 1,
        },
        RejectionCode::StaleFence,
    );
    for ack in [
        FenceAcknowledgement {
            target: old_gate,
            control_version: 2,
        },
        FenceAcknowledgement {
            target: target.clone(),
            control_version: 1,
        },
    ] {
        let mut stale = requested.clone();
        stale
            .run
            .as_mut()
            .unwrap()
            .stop
            .as_mut()
            .unwrap()
            .acknowledgements
            .push(ack);
        let restored: CampaignState =
            serde_json::from_slice(&serde_json::to_vec(&stale).unwrap()).unwrap();
        error(
            &restored,
            Command::ConfirmStopFence,
            RejectionCode::FenceIncomplete,
        );
    }
    let acknowledged = acknowledge(&requested);
    let repeated = acknowledge(&acknowledged);
    assert_eq!(
        repeated
            .run
            .as_ref()
            .unwrap()
            .stop
            .as_ref()
            .unwrap()
            .acknowledgements
            .len(),
        1
    );
    assert_eq!(
        step(&repeated, Command::ConfirmStopFence).control,
        CampaignControl::Stopping
    );
}

#[test]
fn alive_unknown_and_wrong_incarnation_keep_stop_and_edit_locked() {
    for observation in [
        SessionObservation::Alive { observed_at: now() },
        SessionObservation::Unknown {
            reason: "host unreachable".into(),
            last_observed_at: None,
        },
    ] {
        let mut state = with_action(ActionState::Started, true);
        let identity = binding(&state).session;
        state.run.as_mut().unwrap().sessions = vec![SessionRecord {
            identity: identity.clone(),
            attempt_id: "attempt".into(),
            observation,
        }];
        let requested = stop(&state, StopMode::Immediate);
        let reconciled = observe(
            &requested,
            ActionObservation::Reconciled {
                evidence: "effect observed".into(),
            },
        );
        let fenced = step(&acknowledge(&reconciled), Command::ConfirmStopFence);
        error(
            &fenced,
            Command::ConfirmStopped,
            RejectionCode::SessionsNotGone,
        );
        let mut apparent_stopped = fenced.clone();
        apparent_stopped.control = CampaignControl::Stopped;
        assert_eq!(
            edit_guard(&apparent_stopped).unwrap_err().code,
            RejectionCode::SessionsNotGone
        );
        let mut wrong = identity.clone();
        wrong.incarnation = "previous-process".into();
        error(
            &fenced,
            Command::ObserveSession {
                identity: wrong,
                observation: SessionObservation::Gone {
                    observed_at: now(),
                    evidence: "absent".into(),
                },
            },
            RejectionCode::StaleBinding,
        );
        error(
            &fenced,
            Command::ObserveSession {
                identity: identity.clone(),
                observation: SessionObservation::Gone {
                    observed_at: now(),
                    evidence: " ".into(),
                },
            },
            RejectionCode::InvalidState,
        );
        let gone = step(
            &fenced,
            Command::ObserveSession {
                identity,
                observation: SessionObservation::Gone {
                    observed_at: now(),
                    evidence: "reachable host: exact process absent".into(),
                },
            },
        );
        let stopped = step(&gone, Command::ConfirmStopped);
        assert!(edit_guard(&stopped).is_ok());
    }
}

#[test]
fn unreconciled_action_blocks_stopped_and_definition_edit_after_fence() {
    let requested = stop(
        &with_action(ActionState::Started, true),
        StopMode::Immediate,
    );
    let fenced = step(&acknowledge(&requested), Command::ConfirmStopFence);
    error(
        &fenced,
        Command::ConfirmStopped,
        RejectionCode::UnreconciledActions,
    );
    let mut apparent_stopped = fenced.clone();
    apparent_stopped.control = CampaignControl::Stopped;
    assert_eq!(
        edit_guard(&apparent_stopped).unwrap_err().code,
        RejectionCode::UnreconciledActions
    );
    error(
        &apparent_stopped,
        Command::EditDefinition {
            expected_revision: 1,
            definition: input(),
            change: None,
        },
        RejectionCode::UnreconciledActions,
    );
    let unknown = observe(
        &fenced,
        ActionObservation::Unknown {
            reason: "response lost after initiation".into(),
        },
    );
    error(
        &unknown,
        Command::ObserveAction {
            action_id: "action".into(),
            binding: Box::new(binding(&unknown)),
            observation: ActionObservation::NotStarted {
                evidence: "cannot undo a receipt".into(),
            },
        },
        RejectionCode::InvalidState,
    );
    let reconciled = observe(
        &unknown,
        ActionObservation::Reconciled {
            evidence: "same external identity observed".into(),
        },
    );
    let stopped = step(&reconciled, Command::ConfirmStopped);
    assert!(edit_guard(&stopped).is_ok());
    let mut restored = reconciled.clone();
    restored.run.as_mut().unwrap().sessions.clear();
    error(
        &restored,
        Command::ConfirmStopped,
        RejectionCode::SessionsNotGone,
    );
    let recovered = step(
        &restored,
        Command::ObserveSession {
            identity: binding(&restored).session,
            observation: SessionObservation::Gone {
                observed_at: now(),
                evidence: "exact process absent after restore".into(),
            },
        },
    );
    assert_eq!(
        step(&recovered, Command::ConfirmStopped).control,
        CampaignControl::Stopped
    );
}

#[test]
fn action_progression_needs_current_fence_and_direct_initiation_receipt() {
    let mut state = with_action(ActionState::Planned, false);
    let action = state
        .run
        .as_mut()
        .unwrap()
        .actions
        .remove("action")
        .unwrap();
    state = step(
        &state,
        Command::PlanAction {
            action: Box::new(action.clone()),
        },
    );
    error(
        &state,
        Command::PlanAction {
            action: Box::new(action),
        },
        RejectionCode::AlreadyExists,
    );
    for phase in [ActionState::Claimed, ActionState::StartPending] {
        state = step(
            &state,
            Command::AdvanceAction {
                action_id: "action".into(),
                binding: Box::new(binding(&state)),
                state: phase,
            },
        );
        assert!(!state.run.as_ref().unwrap().attempts["attempt"].started);
    }
    error(
        &state,
        Command::AdvanceAction {
            action_id: "action".into(),
            binding: Box::new(binding(&state)),
            state: ActionState::Started,
        },
        RejectionCode::InvalidState,
    );
    error(
        &state,
        Command::ObserveAction {
            action_id: "action".into(),
            binding: Box::new(binding(&state)),
            observation: ActionObservation::Started { receipt: "".into() },
        },
        RejectionCode::InvalidState,
    );
    let started = observe(
        &state,
        ActionObservation::Started {
            receipt: "gate receipt".into(),
        },
    );
    assert!(started.run.as_ref().unwrap().attempts["attempt"].started);
    assert_eq!(
        validate_new_start(&started, &binding(&started))
            .unwrap_err()
            .code,
        RejectionCode::SessionsNotGone
    );
    let requested = stop(&state, StopMode::Immediate);
    error(
        &requested,
        Command::AdvanceAction {
            action_id: "action".into(),
            binding: Box::new(binding(&state)),
            state: ActionState::Claimed,
        },
        RejectionCode::InvalidState,
    );
}

#[test]
fn receipt_replay_and_body_conflict_precede_stale_checks_and_emit_no_events() {
    let state = running();
    let command = envelope(Command::RequestStop {
        expected_revision: 1,
        expected_run: "run".into(),
        mode: StopMode::Immediate,
    });
    let decision = decide_with_receipt(&state, &command, None).unwrap();
    let receipt = Receipt {
        fingerprint: command_fingerprint(&command.command).unwrap(),
        result: Ok(decision.result.clone()),
    };
    let mut newer = decision.state.clone();
    newer.definition_revision += 1;
    newer.event_sequence += 100;
    newer.run.as_mut().unwrap().id = "another-run".into();
    let mut retry = command.clone();
    retry.observed_at += chrono::Duration::seconds(10);
    let replay = decide_with_receipt(&newer, &retry, Some(&receipt)).unwrap();
    assert_eq!(replay.result, decision.result);
    assert_eq!(replay.state, newer);
    assert!(replay.events.is_empty());
    retry.command = Command::RequestStop {
        expected_revision: 1,
        expected_run: "run".into(),
        mode: StopMode::FinishStage,
    };
    assert_eq!(
        decide_with_receipt(&newer, &retry, Some(&receipt))
            .unwrap_err()
            .code,
        RejectionCode::IdempotencyConflict
    );
    let rejected = Receipt {
        fingerprint: receipt.fingerprint,
        result: Err(Rejection::new(RejectionCode::InvalidState)),
    };
    assert_eq!(
        decide_with_receipt(&newer, &command, Some(&rejected))
            .unwrap_err()
            .code,
        RejectionCode::InvalidState
    );
}

#[test]
fn stop_ignores_event_growth_but_still_targets_run_and_definition() {
    let mut state = running();
    let command = envelope(Command::RequestStop {
        expected_revision: 1,
        expected_run: "run".into(),
        mode: StopMode::FinishStage,
    });
    state.event_sequence += 50;
    let requested = decide(&state, &command).unwrap();
    assert_eq!(requested.state.event_sequence, state.event_sequence + 1);
    assert_eq!(
        requested.events[0].kind,
        EventKind::StopRequested {
            mode: StopMode::FinishStage
        }
    );
    state.definition_revision += 1;
    assert_eq!(
        decide(&state, &command).unwrap_err().code,
        RejectionCode::StaleDefinition
    );
    state.definition_revision = 1;
    state.run.as_mut().unwrap().id = "different-run".into();
    assert_eq!(
        decide(&state, &command).unwrap_err().code,
        RejectionCode::StaleRun
    );
}

#[test]
fn edits_supersede_changed_evidence_preserve_main_and_keep_graph_only_evidence() {
    let mut state = prepared();
    state.nodes.get_mut("a").unwrap().evidence_current = true;
    state.nodes.get_mut("a").unwrap().main_integrated = true;
    let mut graph = input();
    graph.nodes[1].dependencies.push("c".into());
    let edited = step(
        &state,
        Command::EditDefinition {
            expected_revision: 1,
            definition: graph.clone(),
            change: None,
        },
    );
    assert_eq!(edited.definition_revision, 2);
    assert!(edited.nodes["a"].evidence_current);
    graph.nodes[0].acceptance[0].description = "New requirement".into();
    error(
        &edited,
        Command::EditDefinition {
            expected_revision: 2,
            definition: graph.clone(),
            change: None,
        },
        RejectionCode::MissingChangeReason,
    );
    let changed = Command::EditDefinition {
        expected_revision: 2,
        definition: graph,
        change: Some(ChangeReason {
            reference: "https://github.com/itismyfield/AgentDesk/issues/7438".into(),
            explanation: "Changed AC".into(),
        }),
    };
    let decision = decide(&edited, &envelope(changed.clone())).unwrap();
    assert!(!decision.state.nodes["a"].evidence_current);
    assert!(decision.state.nodes["a"].main_integrated);
    assert!(
        matches!(&decision.events[0].kind, EventKind::DefinitionEdited { superseded_nodes, .. } if superseded_nodes == &["a".to_string()])
    );
    let mut complete = edited;
    complete.nodes.get_mut("a").unwrap().acceptance_complete = true;
    error(&complete, changed, RejectionCode::CompletedNodeChanged);
}

#[test]
fn execution_is_unavailable_without_a_production_runtime_caller() {
    assert_eq!(execution_readiness(), ExecutionReadiness::Unavailable);
    error(
        &prepared(),
        Command::Start {
            expected_revision: 1,
        },
        RejectionCode::ExecutionUnavailable,
    );
    let mut state = running();
    state.control = CampaignControl::Stopped;
    for command in [
        Command::Resume {
            expected_revision: 1,
            expected_run: "run".into(),
        },
        Command::PrepareRetry {
            expected_revision: 1,
            expected_run: "run".into(),
            node_id: "a".into(),
        },
    ] {
        error(&state, command, RejectionCode::ExecutionUnavailable);
    }
}
