use super::*;
use crate::services::tmux_common as tc;
use crate::services::tui_prompt_dedupe::binding_context::{
    self, BindingContext, PreparedIncarnation,
};
use std::{fs, os::unix::fs::MetadataExt, path::PathBuf};

const ID: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";

fn context(policy: &str) -> PreparedIncarnation {
    let root = crate::config::runtime_root().unwrap();
    let sessions = root.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    PreparedIncarnation::create(BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: format!("readonly-{}", uuid::Uuid::new_v4()),
        channel_id: Some(7308),
        owner_runtime_root: tc::current_tmux_owner_marker(),
        host: Some("test-host".into()),
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some(sessions.canonicalize().unwrap()),
        first_prompt_digest: None,
        source_policy: Some(policy.into()),
        claude_resume: None,
    })
    .unwrap()
}

fn binding(path: PathBuf) -> TuiRuntimeBinding {
    TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::CodexTui,
        output_path: path.display().to_string(),
        session_id: Some(ID.into()),
        relay_output_path: None,
        input_fifo_path: None,
        last_offset: 17,
        relay_last_offset: Some(4),
    }
}

fn insert(context: &BindingContext, binding: &TuiRuntimeBinding) -> Instant {
    let at = Instant::now();
    STATE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .runtime_by_tmux
        .insert(
            context.tmux_session.clone(),
            TimedValue {
                value: binding.clone(),
                recorded_at: at,
            },
        );
    let marker = tc::session_temp_path(&context.tmux_session, "spawn_nonce");
    fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    fs::write(marker, &context.execution_nonce).unwrap();
    at
}

#[test]
fn runtime_observation_distinguishes_missing_expired_and_withheld_without_purging() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state_lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (_root, _env) = binding_context::tests::fixture_after_shared_test_env_lock();
    reset_state_for_tests();
    let prepared = context("shadow");
    let context = &prepared.context;
    let value = binding(context.provider_root.as_ref().unwrap().join("source.jsonl"));
    tc::with_tmux_source_authority(&context.tmux_session, |authority| {
        assert_eq!(
            runtime_observation_under_source_authority(authority),
            RuntimeObservation::Missing
        );
    });
    let recorded_at = insert(context, &value);
    tc::with_tmux_source_authority(&context.tmux_session, |authority| {
        let pin = authority.pin().unwrap();
        assert_eq!(
            runtime_observation_under_source_authority(authority),
            RuntimeObservation::Present(value.clone())
        );
        Instant::advance(SESSION_MAPPING_TTL + Duration::from_secs(1));
        assert_eq!(
            runtime_observation_under_source_authority(authority),
            RuntimeObservation::Expired(value.clone())
        );
        assert!(
            pin.is_current(),
            "observation must not invalidate source authority"
        );
    });
    assert_eq!(
        runtime_binding_recorded_at_for_tests(&context.tmux_session),
        Some(recorded_at)
    );
    Instant::reset();
    fs::remove_file(prepared.path).unwrap();
    tc::with_tmux_source_authority(&context.tmux_session, |authority| {
        assert_eq!(
            runtime_observation_under_source_authority(authority),
            RuntimeObservation::Withheld(value.clone())
        );
    });
    assert_eq!(
        runtime_binding_recorded_at_for_tests(&context.tmux_session),
        Some(recorded_at)
    );
    reset_state_for_tests();
}

#[test]
fn verified_runtime_observation_checks_durable_proof_and_exact_marker_without_publication() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state_lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _env) = binding_context::tests::fixture_after_shared_test_env_lock();
    reset_state_for_tests();
    binding_events::set_test_root(Some(root.path()));
    let prepared = context("verified");
    let context = &prepared.context;
    let path = context
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{ID}.jsonl"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let at = (context.created_at + chrono::Duration::seconds(1)).to_rfc3339();
    fs::write(&path, format!("{}\n", serde_json::json!({"type":"session_meta", "timestamp":at,
        "payload":{"id":ID,"timestamp":at,"cwd":root.path(),"source":"cli","originator":"codex_cli_rs"}}))).unwrap();
    let source = binding_events::SourceId {
        session_id: ID.into(),
        path: path.canonicalize().unwrap(),
        dev: fs::metadata(&path).unwrap().dev(),
        ino: fs::metadata(&path).unwrap().ino(),
    };
    let claim = binding_events::codex::Claim {
        session_id: ID.into(),
        path: Some(source.path.clone()),
        evidence: binding_events::codex::ClaimEvidence::NativeHook {
            event: "session_start".into(),
            source: Some("startup".into()),
            first_prompt_digest: None,
        },
    };
    let fold = tc::with_tmux_source_authority(&context.tmux_session, |_| {
        binding_events::codex::commit_claim(
            context,
            &claim,
            binding_events::codex::Decision::Verified(source),
            || Ok(()),
        )
        .unwrap()
    });
    let proof = fold.verified.unwrap();
    let value = binding(path);
    let at = insert(context, &value);
    let marker =
        tc::session_temp_path(&context.tmux_session, tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT);
    fs::write(
        &marker,
        serde_json::to_vec(&serde_json::json!({"rollout_path":proof.source.path,
        "session_id":ID,"codex_ownership":codex_verified::marker_proof(context, &proof)}))
        .unwrap(),
    )
    .unwrap();
    let before = fs::read(&marker).unwrap();
    tc::with_tmux_source_authority(&context.tmux_session, |authority| {
        let pin = authority.pin().unwrap();
        assert_eq!(
            runtime_observation_under_source_authority(authority),
            RuntimeObservation::Present(value.clone())
        );
        assert!(pin.is_current());
    });
    assert_eq!(fs::read(&marker).unwrap(), before);
    fs::write(&marker, "{}").unwrap();
    tc::with_tmux_source_authority(&context.tmux_session, |authority| {
        assert_eq!(
            runtime_observation_under_source_authority(authority),
            RuntimeObservation::Withheld(value.clone())
        );
    });
    assert_eq!(
        runtime_binding_recorded_at_for_tests(&context.tmux_session),
        Some(at)
    );
    binding_events::forget_channel_for_tests(7308);
    binding_events::set_test_root(None);
    reset_state_for_tests();
}

#[test]
fn a_claude_kind_observation_never_enters_a_cached_codex_filesystem_path() {
    use std::os::unix::fs::PermissionsExt;
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state_lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (_root, _env) = binding_context::tests::fixture_after_shared_test_env_lock();
    reset_state_for_tests();
    let logical = "readonly-codex-kind-boundary";
    let value = binding(PathBuf::from("/nonexistent/test-rollout.jsonl"));
    let at = Instant::now();
    STATE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .runtime_by_tmux
        .insert(
            logical.into(),
            TimedValue {
                value: value.clone(),
                recorded_at: at,
            },
        );
    let sessions = tc::persistent_sessions_dir().unwrap();
    assert!(!sessions.exists(), "fixture must begin without sessions");
    for present in [false, true] {
        if present {
            fs::create_dir_all(&sessions).unwrap();
            fs::set_permissions(&sessions, fs::Permissions::from_mode(0o755)).unwrap();
        }
        tc::with_tmux_source_authority(logical, |authority| {
            assert_eq!(
                runtime_observation_for_kind_under_source_authority(
                    authority,
                    RuntimeHandoffKind::ClaudeTui
                ),
                RuntimeObservation::Withheld(value.clone())
            );
        });
        if present {
            assert_eq!(
                fs::metadata(&sessions).unwrap().permissions().mode() & 0o777,
                0o755,
                "observation cannot chmod another provider's sessions"
            );
            assert_eq!(fs::read_dir(&sessions).unwrap().count(), 0);
        } else {
            assert!(
                !sessions.exists(),
                "observation cannot create another provider's sessions"
            );
        }
        assert_eq!(runtime_binding_recorded_at_for_tests(logical), Some(at));
    }
    reset_state_for_tests();
}
