use super::*;

#[test]
fn retired_after_server_restart_first_ingress_submits_original_to_new_execution_once_pg() {
    let case = Case::launched("handoff-after-root-replaced");
    let intake = IntakeGate::configure(&case);
    let completion = wakes::KickoffCompletion::install(ChannelId::new(CHANNEL));
    case.pass();
    case.fx
        .rt
        .block_on(completion.processed(0, Duration::from_secs(5)));
    assert_eq!(case.fx.row(), Some(HostedState::Retired));
    assert_eq!(case.cleanups(), 1);
    assert_eq!(case.fx.rig.sends().len(), 2);
    let fixture = owner_input((intake, case), |fixture| &fixture.1, None, Input::Discord);
    let HostedRecord::Known(record) = fixture.1.fx.record() else {
        panic!("new Bound execution");
    };
    assert_eq!(record.state, HostedState::Bound);
    assert_ne!(record.execution_nonce, fixture.1.nonce);
    assert_eq!(fixture.1.fx.rig.sends().len(), 4);
}

#[test]
fn retired_ended_journal_counts_an_actual_pre_pane_create_failure_pg() {
    use crate::services::discord::router::message_handler::provider_dispatch::start_failure::{
        Failure, Kind,
    };
    use crate::services::turn_orchestrator::QueuePersistenceContext;
    let case = Case::launched("handoff-create-failure");
    let _intake = IntakeGate::configure(&case);
    let completion = wakes::KickoffCompletion::install(ChannelId::new(CHANNEL));
    case.pass();
    case.fx
        .rt
        .block_on(completion.processed(0, Duration::from_secs(5)));
    assert_eq!(case.fx.row(), Some(HostedState::Retired));
    let channel = ChannelId::new(CHANNEL);
    let mailbox = case.shared.mailbox(channel);
    let claim = case.fx.rt.block_on(async {
        let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, TOKEN, None);
        let (path, namespace) =
            crate::services::turn_orchestrator::v2_queue(&persistence, channel).unwrap();
        mailbox.v2_adopt(path, namespace).await.unwrap();
        let original =
            ingress::original(&ProviderKind::Claude, channel, None, &queued_input()).unwrap();
        mailbox
            .v2_admit_ticket(original.item, original.ticket.as_str().into())
            .await
            .unwrap();
        let logical = case.fx.logical().to_string();
        let key = super::super::super::super::super::session_key(&case.fx.owner);
        let front = mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().unwrap();
                let front = owner.front().unwrap();
                owner.grant_front(&front)?;
                let journal = owner.begin_handoff(&front, logical, key)?;
                owner.handoff_ended(&journal.id)?;
                Ok(front)
            })
            .await
            .unwrap();
        crate::services::discord::queue_io::input_owner::drive::claim_turn(
            &mailbox,
            front.1.message_id,
            &front,
        )
        .await
        .unwrap()
        .unwrap()
    });
    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
    let _registry = case.fx.rig.registry_as_on_this_thread(
        &endpoint.execution_node,
        &endpoint.key,
        &endpoint.socket_path,
        &endpoint.herdr_session,
    );
    let launcher = Arc::new(Launcher::default());
    *launcher.create_fails.lock().unwrap() = Some(HerdrCreateOutcome::NotSent(
        "forced pre-pane create failure".into(),
    ));
    let ports = case.fx.ports(&launcher);
    let row = case.fx.record();
    let _runtime = case.fx.rt.enter();
    let _admission = open_admission();
    let (sender, _) = std::sync::mpsc::channel();
    let error = herdr_turn::execute(
        HerdrTurn {
            pool: &case.fx.pool,
            owner: case.fx.owner.clone(),
            channel_id: CHANNEL,
            endpoint: HerdrLaunchEndpoint {
                execution_node: endpoint.execution_node,
                config_key: endpoint.key,
                socket_addr: endpoint.socket_path.display().to_string(),
                herdr_session: endpoint.herdr_session,
            },
            row: Some(&row),
            prompt: &claim.front().1.text,
            working_dir: case.fx.cwd.path().to_str().unwrap(),
            session_id: None,
            force_fresh: true,
            system_prompt: None,
            model: None,
            hook_endpoint: Some("http://127.0.0.1:1".into()),
            cancel: None,
        },
        &ports,
        sender,
    )
    .unwrap_err();
    assert!(error.contains("admit_create_evidence"), "{error}");
    assert!(error.contains("forced pre-pane create failure"), "{error}");
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    assert_eq!(
        case.fx.rig.sends().len(),
        2,
        "failed launch sends no prompt"
    );
    case.fx.rt.block_on(async {
        assert_eq!(
            Failure::claimed(&mailbox, &claim)
                .commit(Kind::Opaque, error, Ok(None), None, channel, None)
                .await
                .unwrap(),
            Some(false)
        );
        let state = mailbox
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap();
        assert_eq!(state["unfinished_handoff"]["failures"], 1);
        assert_eq!(state["items"].as_array().unwrap().len(), 1);
        assert!(state["retained_inputs"].as_array().unwrap().is_empty());
    });
}
