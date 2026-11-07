//! Retirement reaches current absence; one normal input creates and attaches its own source.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use crate::services::claude::herdr_turn::{self, AttachRequest, HerdrTurn, HerdrTurnPorts};
use crate::services::claude_tui::hook_server::{
    HookEvent, HookServerState, hook_receiver_router_with_state,
};
use crate::services::discord::recovery_engine::herdr_reader::SocketHerdrReader;
use crate::services::discord::tui_prompt_relay::herdr_source::attach_launched_herdr_source;
use crate::services::discord::turn_presence::activity::{self, Activity};
use crate::services::herdr_launch::HerdrLaunchHost;
use crate::services::provider_auth_profile::ProviderAuthOverlay;
use crate::services::tui_prompt_dedupe::binding_events;
use real::missing_marker_rig::LOGICAL;
use std::path::PathBuf;
use std::time::Duration;

struct Ports {
    pool: PgPool,
    hooks: HookServerState,
    overlay: ProviderAuthOverlay,
}
impl HerdrTurnPorts for Ports {
    fn launch_host(&self) -> Option<Arc<dyn HerdrLaunchHost>> {
        herdr_turn::boot_launch_host()
    }
    fn hook_events(&self) -> tokio::sync::broadcast::Receiver<HookEvent> {
        self.hooks.subscribe()
    }
    fn auth_overlay(&self, _: &str) -> Result<ProviderAuthOverlay, String> {
        Ok(self.overlay.clone())
    }
    fn attach(&self, request: &AttachRequest<'_>) -> Result<bool, String> {
        let reader = SocketHerdrReader::of(request.record).ok_or("missing reader")?;
        match tokio::runtime::Handle::current().block_on(attach_launched_herdr_source(
            &self.pool,
            request.owner,
            CHANNEL,
            &request.record.execution_nonce,
            request.session_id,
            request.transcript,
            &reader,
        )) {
            HerdrSourceAttach::Published { bound, .. } => Ok(bound),
            other => Err(format!("attach: {other:?}")),
        }
    }
    fn confirm_bound(
        &self,
        _: &HostedOwner,
        record: &HostedExecution,
        _: &HerdrTarget,
    ) -> Result<(), String> {
        let reader = SocketHerdrReader::of(record).ok_or("missing reader")?;
        let row =
            tokio::runtime::Handle::current().block_on(hosted_execution::load_hosted_execution_pg(
                &self.pool,
                HostedLookupKey::SessionKey(&format!("claude/{TOKEN}/mac-mini:{LOGICAL}")),
            ));
        crate::services::discord::recovery_engine::host_reconcile::reconcile_hosted(&row, &reader)
            .admits_reconnect()
            .then_some(())
            .ok_or("reconcile refused".into())
    }
}

#[test]
fn mm_retire_presence_then_normal_input_finds_source_pg() {
    if !child(
        "mm_retire_presence_then_normal_input_finds_source_pg",
        module_path!(),
    ) {
        return;
    }
    let mut case = Case::new();
    let other_cwd = case.real.work("same-cwd");
    let other = case.rt.block_on(real::launch(
        &mut case.real,
        &case.pool,
        TOKEN,
        CHANNEL + 1,
        "AgentDesk-claude-mm-other",
        other_cwd,
    ));
    let other_record = case.rt.block_on(real::stored(&case.pool, &other.key));
    let root = crate::config::runtime_root().unwrap();
    let _binding = activity::tests::BindingRoot::enter(&root);
    let _confirmed = crate::services::tui_o::turn_mode::TestConfirmation::new(CHANNEL);
    let _namespace =
        crate::services::platform::tmux::session_inventory::tests::ChannelNamespace::new(
            CHANNEL,
            root.join("absence-private.sock"),
        );
    case.rt.block_on(
        crate::services::discord::host_defer_gate::tests::map_channel(
            &case.shared,
            ChannelId::new(CHANNEL),
            "missing-marker-test",
        ),
    );
    let bound = case.observation();
    assert!(
        case.rt
            .block_on(
                crate::services::discord::admin_host_guard::managed_reset_refusal(
                    &case.shared,
                    &ProviderKind::Claude,
                    ChannelId::new(CHANNEL),
                    true,
                    true,
                    None
                )
            )
            .is_some()
    );
    assert_eq!(case.observation(), bound);
    let local = crate::services::discord::adk_session::build_namespaced_session_key(
        TOKEN,
        &ProviderKind::Claude,
        LOGICAL,
    );
    case.rt.block_on(async {
        sqlx::query("INSERT INTO session_key_aliases (session_key, session_id) SELECT $1,id FROM sessions WHERE session_key=$2 ON CONFLICT DO NOTHING")
            .bind(local).bind(&case.execution.key).execute(&case.pool).await.unwrap();
    });
    case.ended();
    case.lose_marker();
    let unknown = case.rt.block_on(activity::reading_now(
        &case.shared,
        &ProviderKind::Claude,
        ChannelId::new(CHANNEL),
    ));
    assert_eq!(unknown.observed.activity, Activity::Unknown);
    assert!(
        case.rt
            .block_on(unknown.publish_if_current(|| ()))
            .is_some()
    );
    case.pass();
    let record = case.record();
    assert_eq!(record.state, HostedState::Retired);
    let absence = case.rt.block_on(activity::reading_now(
        &case.shared,
        &ProviderKind::Claude,
        ChannelId::new(CHANNEL),
    ));
    assert_eq!(
        (absence.observed.activity, absence.observed.reason),
        (Activity::Idle, "no_session")
    );
    assert!(
        case.rt
            .block_on(absence.publish_if_current(|| ()))
            .is_some()
    );

    let home = root.join("claude-home");
    std::fs::create_dir_all(&home).unwrap();
    let cwd = case.real.work("same-cwd");
    let project =
        crate::services::claude_tui::transcript_tail::claude_project_dir_for_cwd(&cwd, Some(&home))
            .unwrap();
    let bin = root.join("fixture-claude");
    std::fs::write(
        &bin,
        include_str!("../../../../tests/fixtures/missing_marker_provider.py"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _bin = case.native_binary(&bin);
    let state = HookServerState::new_with_claude_projects_root(home.join("projects"));
    let (url, server) = case.rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = hook_receiver_router_with_state(state.clone());
        (
            url,
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
        )
    });
    let mut overlay = ProviderAuthOverlay::default_for(ProviderKind::Claude);
    overlay.home = Some(home.clone());
    overlay
        .env
        .insert("CLAUDE_CONFIG_DIR".into(), home.display().to_string());
    overlay
        .env
        .insert("ADK_MM_PROJECT_DIR".into(), project.display().to_string());
    overlay.env.insert("ADK_MM_HOOK_URL".into(), url.clone());
    let received = root.join("received-prompt");
    overlay
        .env
        .insert("ADK_MM_RECEIVED".into(), received.display().to_string());
    let ports = Ports {
        pool: case.pool.clone(),
        hooks: state,
        overlay,
    };
    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
    let endpoint = crate::services::herdr_launch::HerdrLaunchEndpoint {
        execution_node: endpoint.execution_node,
        config_key: endpoint.key,
        socket_addr: endpoint.socket_path.display().to_string(),
        herdr_session: endpoint.herdr_session,
    };
    let _admission = crate::services::herdr_admission::force_for_test(
        crate::services::herdr_admission::Admission::new(true, Some(root.join("no-stop"))),
    );
    let cancel = Arc::new(CancelToken::new());
    let cancel_timeout = cancel.clone();
    let timeout = case.rt.spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        cancel_timeout
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    });
    let (sender, receiver) = std::sync::mpsc::channel();
    let _enter = case.rt.enter();
    let result = crate::services::tui_o::exact_submission::dispatch(|| {
        herdr_turn::execute(
            HerdrTurn {
                pool: &case.pool,
                owner: record.owner.clone(),
                channel_id: CHANNEL,
                endpoint,
                row: Some(&HostedRecord::Known(record.clone())),
                prompt: "one new normal input",
                working_dir: cwd.to_str().unwrap(),
                session_id: None,
                force_fresh: true,
                system_prompt: None,
                model: None,
                hook_endpoint: Some(url),
                cancel: Some(cancel.clone()),
            },
            &ports,
            sender,
        )
    });
    timeout.abort();
    server.abort();
    assert!(result.is_ok(), "normal input: {result:?}");
    assert!(
        !cancel.cancelled.load(std::sync::atomic::Ordering::Acquire),
        "a timeout cannot count as a successful turn"
    );
    assert_eq!(
        std::fs::read_to_string(received).unwrap(),
        "one new normal input"
    );
    let messages: Vec<_> = receiver.try_iter().collect();
    assert!(!messages.is_empty());
    let next = case.record();
    assert_eq!(next.state, HostedState::Bound);
    let expected = next.expected.as_ref().unwrap();
    case.real
        .recorded
        .push((expected.root.pid, expected.root.start.clone()));
    case.real.recorded.push((
        expected.provider_process.pid,
        expected.provider_process.start.clone(),
    ));
    assert_ne!(next.execution_nonce, record.execution_nonce);
    assert_ne!(next.location.as_ref().unwrap().pane_id, other.pane);
    assert_eq!(
        case.rt.block_on(real::stored(&case.pool, &other.key)),
        other_record
    );
    assert!(real::alive(&other.provider));
    assert_ne!(next.location.as_ref().unwrap().pane_id, case.execution.pane);
    let binding = dedupe::runtime_binding_for_tmux_session(LOGICAL).unwrap();
    let source = PathBuf::from(binding.output_path);
    assert!(source.starts_with(&project) && source.exists());
    let events = binding_events::binding_events_since(CHANNEL, 0).unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.execution_nonce.as_deref() == Some(&next.execution_nonce))
    );
    use crate::services::tui_o::writer::binding::{
        BindingEvents, BindingRecord, BindingTarget, ChannelBindingLog,
    };
    let writer_events = ChannelBindingLog::new(
        CHANNEL,
        crate::services::tui_o::shadow::ShadowProvider::Claude,
    )
    .binding_events_since(CHANNEL, 0)
    .unwrap();
    assert!(
        writer_events
            .iter()
            .any(|event| event.execution_nonce == next.execution_nonce
                && match &event.record {
                    BindingRecord::Bound {
                        new: BindingTarget::Source(id),
                        ..
                    }
                    | BindingRecord::Resolved { source: id, .. } => id.path == source,
                    _ => false,
                }),
        "the production writer input port must find this execution's own source"
    );
    assert!(
        std::fs::read_to_string(&source)
            .unwrap()
            .contains("Recovered managed execution reply.")
    );
    case.rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let observed = activity::reading_now(
                    &case.shared,
                    &ProviderKind::Claude,
                    ChannelId::new(CHANNEL),
                )
                .await;
                if observed.source() == Some(source.to_str().unwrap())
                    && observed.publish_if_current(|| ()).await.is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("current presence must observe the new execution's source");
    });
}
