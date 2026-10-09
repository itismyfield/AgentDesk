#![cfg(unix)]

use super::*;
use crate::config::TestEnvVarGuard;
use crate::services::tui_prompt_dedupe::binding_events::codex::{Claim, ClaimEvidence, Decision};
use std::{fs, path::PathBuf};

const CHANNEL: u64 = 584_506;
const NATIVE: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";

struct Fixture {
    context: BindingContext,
    root: tempfile::TempDir,
    _env: [TestEnvVarGuard; 2],
    _env_lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

impl Fixture {
    fn new() -> Self {
        let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let (root, env) = binding_context::tests::fixture_after_shared_test_env_lock();
        binding_events::set_test_root(Some(root.path()));
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: "a".repeat(32),
            tmux_session: format!("historical-{}", uuid::Uuid::new_v4()),
            channel_id: Some(CHANNEL),
            owner_runtime_root: root.path().display().to_string(),
            host: Some("original-host".into()),
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(sessions),
            first_prompt_digest: Some(format!("sha256:{}", "a".repeat(64))),
            source_policy: Some("verified".into()),
        };
        Self {
            context,
            root,
            _env: env,
            _env_lock: env_lock,
        }
    }

    fn commit(&self, resolved: bool) -> binding_events::BindingEvent {
        let path = self
            .context
            .provider_root
            .as_ref()
            .unwrap()
            .join("rollout.jsonl");
        fs::write(&path, "{\"type\":\"session_meta\"}\n").unwrap();
        let (dev, ino) =
            crate::services::tui_o::shadow::capture::file_identity(&fs::metadata(&path).unwrap());
        let source = binding_events::SourceId {
            session_id: NATIVE.into(),
            path: path.clone(),
            dev,
            ino,
        };
        let claim = Claim {
            session_id: NATIVE.into(),
            path: Some(path),
            evidence: ClaimEvidence::NativeHook {
                event: "session_start".into(),
                source: Some("startup".into()),
                first_prompt_digest: None,
            },
        };
        tc::with_tmux_source_authority(&self.context.tmux_session, |_| {
            if resolved {
                binding_events::codex::commit_claim(
                    &self.context,
                    &claim,
                    Decision::Pending,
                    || Ok(()),
                )
                .unwrap();
            }
            binding_events::codex::commit_claim(
                &self.context,
                &claim,
                Decision::Verified(source),
                || Ok(()),
            )
            .unwrap();
        });
        binding_events::binding_events_since(CHANNEL, 0)
            .unwrap()
            .pop()
            .unwrap()
    }

    fn marker(&self) -> PathBuf {
        PathBuf::from(tc::session_temp_path(
            &self.context.tmux_session,
            "spawn_nonce",
        ))
    }

    fn bytes(&self) -> Vec<u8> {
        fs::read(
            self.root
                .path()
                .join(binding_events::BINDING_EVENTS_DIR)
                .join(format!("{CHANNEL}.log")),
        )
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        codex_verified::clear_permissions_for_tests();
        binding_events::forget_channel_for_tests(CHANNEL);
        binding_events::set_test_root(None);
    }
}

#[test]
fn historical_proof_survives_current_nonce_and_permission_change_read_only() {
    let fixture = Fixture::new();
    let event = fixture.commit(false);
    fs::create_dir_all(fixture.marker().parent().unwrap()).unwrap();
    fs::write(fixture.marker(), "b".repeat(32)).unwrap();
    codex_verified::set_permission_for_tests(&fixture.context, DeliveryPermission::Cancelled);
    let bytes = fixture.bytes();
    let execution = historical_execution(&fixture.context, &event).unwrap();
    assert_eq!(execution.execution_nonce, fixture.context.execution_nonce);
    assert_eq!(execution.proof_seq, event.seq);
    assert_eq!(execution.tmux_session, fixture.context.tmux_session);
    assert_eq!(
        execution.owner_runtime_root,
        fixture.context.owner_runtime_root
    );
    assert_eq!(execution.source.session_id, NATIVE);
    assert_eq!(
        fixture.bytes(),
        bytes,
        "history lookup must remain read-only"
    );
    assert_eq!(
        fs::read_to_string(fixture.marker()).unwrap(),
        "b".repeat(32)
    );
}

#[test]
fn historical_proof_rejects_event_identity_target_and_sequence_changes() {
    let fixture = Fixture::new();
    let event = fixture.commit(true);
    assert!(historical_execution(&fixture.context, &event).is_ok());
    let mut changed = event.clone();
    changed.execution_nonce = Some("b".repeat(32));
    assert!(historical_execution(&fixture.context, &changed).is_err());
    changed = event.clone();
    changed.channel_id += 1;
    assert!(historical_execution(&fixture.context, &changed).is_err());
    changed = event.clone();
    changed.provider = "claude".into();
    assert!(historical_execution(&fixture.context, &changed).is_err());
    changed = event.clone();
    changed.tmux_session.push_str("-new");
    assert!(historical_execution(&fixture.context, &changed).is_err());
    for seq in [0, event.seq - 1, event.seq + 1] {
        changed = event.clone();
        changed.seq = seq;
        assert!(historical_execution(&fixture.context, &changed).is_err());
    }
    let binding_events::BindingTarget::Resolved { source, .. } = &event.new else {
        panic!("fixture must commit a real resolution");
    };
    let mut wrong_sources = Vec::new();
    let mut wrong_source = source.clone();
    wrong_source.session_id = "019e660d-4859-7522-9cee-8ba7c4e7c744".into();
    wrong_sources.push(wrong_source);
    wrong_source = source.clone();
    wrong_source.path = fixture.root.path().join("other-rollout.jsonl");
    wrong_sources.push(wrong_source);
    wrong_source = source.clone();
    wrong_source.dev += 1;
    wrong_sources.push(wrong_source);
    wrong_source = source.clone();
    wrong_source.ino += 1;
    wrong_sources.push(wrong_source);
    for source in wrong_sources {
        changed = event.clone();
        changed.new = binding_events::BindingTarget::Resolved {
            source,
            pending_seq: 1,
        };
        assert!(historical_execution(&fixture.context, &changed).is_err());
    }
    changed.new = binding_events::BindingTarget::Source(source.clone());
    assert!(historical_execution(&fixture.context, &changed).is_err());
    changed.new = binding_events::BindingTarget::Resolved {
        source: source.clone(),
        pending_seq: 99,
    };
    assert!(historical_execution(&fixture.context, &changed).is_err());
    changed.new = binding_events::BindingTarget::Pending {
        payload_session_id: NATIVE.into(),
        payload_transcript_path: None,
    };
    assert!(historical_execution(&fixture.context, &changed).is_err());
}

#[test]
fn historical_proof_requires_the_complete_original_ownership_envelope() {
    let fixture = Fixture::new();
    let event = fixture.commit(false);
    let mut contexts = Vec::new();
    let mut changed = fixture.context.clone();
    changed.owner_runtime_root.push_str("/other");
    contexts.push(changed);
    changed = fixture.context.clone();
    changed.expected_native_session_id = Some(NATIVE.into());
    contexts.push(changed);
    changed = fixture.context.clone();
    changed.launch_mode = "resume".into();
    contexts.push(changed);
    changed = fixture.context.clone();
    changed.provider_root = Some(fixture.root.path().join("other-sessions"));
    contexts.push(changed);
    changed = fixture.context.clone();
    changed.host = Some("other-host".into());
    contexts.push(changed);
    changed = fixture.context.clone();
    changed.first_prompt_digest = Some(format!("sha256:{}", "b".repeat(64)));
    contexts.push(changed);
    changed = fixture.context.clone();
    changed.created_at += chrono::Duration::seconds(1);
    contexts.push(changed);
    changed = fixture.context.clone();
    changed.source_policy = None;
    contexts.push(changed);
    for context in contexts {
        assert!(historical_execution(&context, &event).is_err());
    }
    let path = fixture
        .root
        .path()
        .join(binding_events::BINDING_EVENTS_DIR)
        .join(format!("{CHANNEL}.log"));
    let mut logged: serde_json::Value = serde_json::from_slice(&fixture.bytes()).unwrap();
    logged.as_object_mut().unwrap().remove("codex_ownership");
    fs::write(path, format!("{logged}\n")).unwrap();
    assert!(
        historical_execution(&fixture.context, &event).is_err(),
        "old unverified event is not provenance"
    );
}
