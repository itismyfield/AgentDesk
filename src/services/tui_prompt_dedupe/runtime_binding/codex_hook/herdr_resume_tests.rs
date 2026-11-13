use super::*;
use crate::services::tui_prompt_dedupe::binding_context::{
    CapturedContext, ObservedHookProcess, PreparedIncarnation,
};
use serde_json::json;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

struct Fixture {
    _env: [crate::config::TestEnvVarGuard; 2],
    root: tempfile::TempDir,
    context: BindingContext,
    source: binding_events::SourceId,
    offset: u64,
    _lock: std::sync::MutexGuard<'static, ()>,
    _env_lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

impl Fixture {
    fn new(resume: bool, intent: bool) -> Self {
        let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let lock = super::super::super::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (root, env) = binding_context::tests::fixture_after_shared_test_env_lock();
        reset_state_for_tests();
        binding_events::set_test_root(Some(root.path()));
        let id = uuid::Uuid::new_v4().to_string();
        let path = root
            .path()
            .join(format!("sessions/rollout-2026-11-13T10-00-00-{id}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let header = json!({"type":"session_meta","payload":{
            "id":id,"source":"cli","cwd":root.path(),"originator":"codex_cli_rs"}});
        std::fs::write(&path, format!("{header}\n")).unwrap();
        let path = path.canonicalize().unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let offset = metadata.len();
        let source = binding_events::SourceId {
            session_id: id.clone(),
            path: path.clone(),
            dev: metadata.dev(),
            ino: metadata.ino(),
        };
        let context: BindingContext = serde_json::from_value(json!({
            "schema":1,"provider":"codex","created_at":chrono::Utc::now(),
            "execution_nonce":uuid::Uuid::new_v4().simple().to_string(),
            "tmux_session":"herdr-resume-hook","channel_id":812812,
            "owner_runtime_root":crate::services::tmux_common::current_tmux_owner_marker(),
            "host":null,"expected_native_session_id":resume.then_some(id),
            "launch_mode":if resume {"resume"} else {"fresh"},
            "provider_root":root.path().join("sessions"),
            "resume_start":intent.then(|| json!({"operation":uuid::Uuid::new_v4().to_string(),
                "path":path,"dev":metadata.dev(),"ino":metadata.ino(),"offset":offset}))
        }))
        .unwrap();
        PreparedIncarnation::create(context.clone()).unwrap();
        std::fs::write(
            crate::services::tmux_common::session_temp_path(&context.tmux_session, "spawn_nonce"),
            &context.execution_nonce,
        )
        .unwrap();
        let fixture = Self {
            _env: env,
            root,
            context,
            source,
            offset,
            _lock: lock,
            _env_lock: env_lock,
        };
        fixture.restore_registry();
        fixture
    }

    fn restore_registry(&self) {
        reset_state_for_tests();
        install_herdr_execution(&self.context.tmux_session, &self.context.execution_nonce);
        register_codex_herdr_placeholder(&self.context.tmux_session, 812812);
    }

    fn hook(&self, source: &binding_events::SourceId) -> IngressOutcome {
        self.hook_reason(
            source,
            if self.context.launch_mode == "resume" {
                "resume"
            } else {
                "startup"
            },
        )
    }

    fn hook_reason(&self, source: &binding_events::SourceId, reason: &str) -> IngressOutcome {
        // The seed's later process reads the immutable incarnation rather than the launch caller.
        let context =
            binding_context::execution_context("codex", &self.context.execution_nonce).unwrap();
        let envelope = HookBindingEnvelope {
            context: CapturedContext::Captured(context.into()),
            observed: ObservedHookProcess::default(),
        };
        let hook = HookSignal {
            event: "session_start".into(),
            source: Some(reason.into()),
            transcript_path: Some(source.path.display().to_string()),
            received_at: chrono::Utc::now(),
            published_at: None,
        };
        observe_codex_herdr_hook(&source.session_id, &hook, Some(&envelope)).unwrap()
    }

    fn binding(&self) -> TuiRuntimeBinding {
        super::super::super::runtime_binding_for_tmux_session(&self.context.tmux_session).unwrap()
    }

    fn marker(&self) -> Option<crate::services::codex_tui::session::CodexTuiRolloutMarker> {
        session::read_codex_tui_rollout_marker(&self.context.tmux_session)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        release_herdr_execution(&self.context.tmux_session, &self.context.execution_nonce);
        binding_events::set_test_root(None);
        reset_state_for_tests();
    }
}

#[test]
fn resumed_herdr_hook_keeps_durable_start_after_registry_restore() {
    let fixture = Fixture::new(true, true);
    use std::io::Write;
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&fixture.source.path)
            .unwrap(),
        "{}",
        json!({"type":"event_msg","payload":{"type":"session_resumed"}})
    )
    .unwrap();
    assert!(std::fs::metadata(&fixture.source.path).unwrap().len() > fixture.offset);
    fixture.restore_registry();
    assert_eq!(
        fixture.hook(&fixture.source),
        IngressOutcome::Durable(DurableKind::Adopted)
    );
    assert_eq!(
        fixture.marker().unwrap().rollout_start_offset,
        Some(fixture.offset)
    );
    let binding = fixture.binding();
    assert_eq!(binding.last_offset, fixture.offset);
    assert_eq!(binding.relay_last_offset, Some(fixture.offset));
    let events = binding_events::binding_events_since(812812, 0).unwrap();
    assert_eq!(
        events.last().unwrap().cause,
        binding_events::BindingCause::Resume
    );
    fixture.restore_registry();
    assert_eq!(
        fixture.hook(&fixture.source),
        IngressOutcome::Durable(DurableKind::Adopted)
    );
    assert_eq!(
        fixture.marker().unwrap().rollout_start_offset,
        Some(fixture.offset)
    );
    assert_eq!(fixture.binding().last_offset, fixture.offset);
    assert_eq!(fixture.binding().relay_last_offset, Some(fixture.offset));
}

#[test]
fn resumed_herdr_hook_without_explicit_start_leaves_source_unpublished() {
    let fixture = Fixture::new(true, false);
    assert_eq!(
        fixture.hook(&fixture.source),
        IngressOutcome::NotApplicable(NotApplicableReason::CodexSourceRejected)
    );
    assert!(fixture.marker().is_none());
    assert!(fixture.binding().output_path.is_empty());
    assert!(
        binding_events::binding_events_since(812812, 0)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn fresh_herdr_hook_preserves_zero_start() {
    let fixture = Fixture::new(false, false);
    assert_eq!(
        fixture.hook(&fixture.source),
        IngressOutcome::Durable(DurableKind::Adopted)
    );
    assert_eq!(fixture.marker().unwrap().rollout_start_offset, Some(0));
    assert_eq!(fixture.binding().last_offset, 0);
}

#[test]
fn resumed_herdr_hook_rejects_another_valid_native_source_before_recording() {
    let fixture = Fixture::new(true, true);
    let id = uuid::Uuid::new_v4().to_string();
    let path: PathBuf = fixture
        .root
        .path()
        .join(format!("sessions/rollout-{id}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{}\n",
            json!({"type":"session_meta","payload":{
        "id":id,"source":"cli","cwd":fixture.root.path(),"originator":"codex_cli_rs"}})
        ),
    )
    .unwrap();
    let metadata = std::fs::metadata(&path).unwrap();
    let other = binding_events::SourceId {
        session_id: id,
        path,
        dev: metadata.dev(),
        ino: metadata.ino(),
    };
    assert_eq!(
        fixture.hook(&other),
        IngressOutcome::NotApplicable(NotApplicableReason::CodexSourceRejected)
    );
    assert!(fixture.marker().is_none());
    assert!(
        binding_events::binding_events_since(812812, 0)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn resumed_herdr_hook_preserves_a_later_native_fork_source() {
    let fixture = Fixture::new(true, true);
    assert_eq!(
        fixture.hook(&fixture.source),
        IngressOutcome::Durable(DurableKind::Adopted)
    );
    let id = uuid::Uuid::new_v4().to_string();
    let path = fixture
        .root
        .path()
        .join(format!("sessions/rollout-{id}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{}\n",
            json!({"type":"session_meta","payload":{
                "id":id,"source":"cli","cwd":fixture.root.path(),"originator":"codex_cli_rs"}})
        ),
    )
    .unwrap();
    let metadata = std::fs::metadata(&path).unwrap();
    let fork = binding_events::SourceId {
        session_id: id,
        path,
        dev: metadata.dev(),
        ino: metadata.ino(),
    };
    assert_eq!(
        fixture.hook_reason(&fork, "fork"),
        IngressOutcome::Durable(DurableKind::Adopted)
    );
    assert_eq!(
        fixture.binding().session_id.as_deref(),
        Some(fork.session_id.as_str())
    );
    assert_eq!(fixture.binding().last_offset, 0);
    assert_eq!(fixture.marker().unwrap().rollout_start_offset, Some(0));
    let history = binding_events::binding_events_since(812812, 0).unwrap();
    assert_eq!(
        history.last().unwrap().cause,
        binding_events::BindingCause::Fork
    );
}
