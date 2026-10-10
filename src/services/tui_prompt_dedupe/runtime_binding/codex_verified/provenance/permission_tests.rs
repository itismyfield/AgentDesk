#![cfg(unix)]

use super::*;
use crate::config::TestEnvVarGuard;
use crate::services::tmux_common as tc;
use crate::services::tui_o::shadow::capture::{file_identity, renumber};
use crate::services::tui_prompt_dedupe::binding_context;
use crate::services::tui_prompt_dedupe::binding_events::codex::{Claim, ClaimEvidence, Decision};
use sha2::{Digest, Sha256};
use std::{fs, io::Write};

const CHANNEL: u64 = 584_507;
const NATIVE: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";

struct Fixture {
    context: BindingContext,
    cursor: Cursor,
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
        let path = sessions.join("rollout.jsonl");
        let header = serde_json::json!({"type":"session_meta","payload":{
            "id":NATIVE,"cwd":"/native","source":"cli","originator":"codex-tui"
        }})
        .to_string()
            + "\n";
        fs::write(&path, &header).unwrap();
        let (dev, ino) = file_identity(&fs::metadata(&path).unwrap());
        let cursor = Cursor {
            source: SourceId {
                session_id: NATIVE.into(),
                path,
                dev,
                ino,
            },
            captured_through: header.len() as u64,
            prefix_hash: hex::encode(Sha256::digest(header.as_bytes())),
            retired: false,
        };
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: "a".repeat(32),
            tmux_session: format!("native-floor-{}", uuid::Uuid::new_v4()),
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
            cursor,
            root,
            _env: env,
            _env_lock: env_lock,
        }
    }

    fn event(&self) -> binding_events::BindingEvent {
        let mut source = self.cursor.source.clone();
        (source.dev, source.ino) = file_identity(&fs::metadata(&source.path).unwrap());
        let claim = Claim {
            session_id: NATIVE.into(),
            path: Some(source.path.clone()),
            evidence: ClaimEvidence::NativeHook {
                event: "session_start".into(),
                source: Some("startup".into()),
                first_prompt_digest: None,
            },
        };
        tc::with_tmux_source_authority(&self.context.tmux_session, |_| {
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

    fn identity(&self) -> HistoricalNativeIdentity {
        HistoricalNativeIdentity::from_historical(
            &self.context,
            &self.event(),
            std::slice::from_ref(&self.cursor),
        )
        .unwrap()
    }

    fn append(&self, bytes: &[u8]) {
        fs::OpenOptions::new()
            .append(true)
            .open(&self.cursor.source.path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        binding_events::forget_channel_for_tests(CHANNEL);
        binding_events::set_test_root(None);
    }
}

#[test]
fn historical_identity_preserves_proof_dev_and_unique_canonical_cursor_after_renumber() {
    let fixture = Fixture::new();
    let _renumber = renumber::shift(&fixture.cursor.source.path, 0x55);
    let event = fixture.event();
    let original = codex_policy::historical_execution(&fixture.context, &event).unwrap();
    let identity = HistoricalNativeIdentity::from_historical(
        &fixture.context,
        &event,
        std::slice::from_ref(&fixture.cursor),
    )
    .unwrap();
    assert_ne!(original.source.dev, fixture.cursor.source.dev);
    assert_eq!(identity.execution(), &original);
    assert_eq!(identity.canonical_source(), &fixture.cursor.source);
    assert_eq!(identity.canonical_cursor(), &fixture.cursor);
    let floor =
        NativeSubmitFloor::capture_before_submit(identity.execution(), identity.canonical_cursor())
            .unwrap();
    assert_eq!(floor.canonical_source(), &fixture.cursor.source);
    assert_eq!(
        floor.verify_for(&original),
        Ok(fixture.cursor.captured_through)
    );
    let mut restatted = original;
    restatted.source = fixture.cursor.source.clone();
    assert_eq!(
        floor.verify_for(&restatted),
        Err("native_floor_execution_mismatch")
    );
}

#[test]
fn historical_identity_requires_one_cursor_and_its_original_prefix() {
    let fixture = Fixture::new();
    let event = fixture.event();
    assert_eq!(
        HistoricalNativeIdentity::from_historical(&fixture.context, &event, &[]).unwrap_err(),
        "canonical_native_cursor_unavailable"
    );
    let mut alias = fixture.cursor.clone();
    alias.source.dev ^= 0x55;
    assert_eq!(
        HistoricalNativeIdentity::from_historical(
            &fixture.context,
            &event,
            &[fixture.cursor.clone(), alias],
        )
        .unwrap_err(),
        "canonical_native_cursor_ambiguous"
    );
    let bytes = fs::read_to_string(&fixture.cursor.source.path).unwrap();
    fs::write(
        &fixture.cursor.source.path,
        bytes.replace("/native", "/nativf"),
    )
    .unwrap();
    assert_eq!(
        HistoricalNativeIdentity::from_historical(
            &fixture.context,
            &event,
            std::slice::from_ref(&fixture.cursor),
        )
        .unwrap_err(),
        "native_floor_prefix_changed"
    );
}

#[test]
fn native_floor_keeps_submit_eof_after_row_prompt_end_and_later_append() {
    let fixture = Fixture::new();
    let identity = fixture.identity();
    let floor =
        NativeSubmitFloor::capture_before_submit(identity.execution(), identity.canonical_cursor())
            .unwrap();
    fixture.append(
        b"{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\"}}\n",
    );
    let prompt_end = fs::metadata(&fixture.cursor.source.path).unwrap().len();
    fixture.append(
        b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn\"}}\n",
    );
    assert_ne!(fixture.cursor.captured_through, prompt_end);
    assert_eq!(
        floor.verify_for(identity.execution()),
        Ok(fixture.cursor.captured_through)
    );
}

#[test]
fn native_floor_rejects_torn_eof_and_rechecks_prefix_after_capture() {
    let fixture = Fixture::new();
    let identity = fixture.identity();
    let floor =
        NativeSubmitFloor::capture_before_submit(identity.execution(), identity.canonical_cursor())
            .unwrap();
    fixture.append(b"{\"type\":\"event_msg\"");
    assert_eq!(NativeSubmitFloor::capture_before_submit(
        identity.execution(), identity.canonical_cursor(),
    ).unwrap_err(), "incomplete_native_floor_record");
    let bytes = fs::read_to_string(&fixture.cursor.source.path).unwrap();
    fs::write(
        &fixture.cursor.source.path,
        bytes.replace("/native", "/nativf"),
    )
    .unwrap();
    assert_eq!(
        floor.verify_for(identity.execution()),
        Err("native_floor_prefix_changed")
    );
}

#[test]
fn native_floor_rejects_wrapper_source_and_wrong_execution_relationship() {
    let fixture = Fixture::new();
    let identity = fixture.identity();
    let mut wrong = identity.execution().clone();
    wrong.source.path = fixture.root.path().join("wrapper.jsonl");
    assert_eq!(
        NativeSubmitFloor::capture_before_submit(&wrong, &fixture.cursor).unwrap_err(),
        "native_floor_execution_mismatch"
    );
    let floor =
        NativeSubmitFloor::capture_before_submit(identity.execution(), identity.canonical_cursor())
            .unwrap();
    wrong = identity.execution().clone();
    wrong.execution_nonce = "b".repeat(32);
    assert_eq!(
        floor.verify_for(&wrong),
        Err("native_floor_execution_mismatch")
    );
    let wrapper = b"{\"type\":\"message\",\"role\":\"user\",\"content\":\"prompt\"}\n";
    fs::write(&fixture.cursor.source.path, wrapper).unwrap();
    let mut cursor = fixture.cursor.clone();
    cursor.captured_through = wrapper.len() as u64;
    cursor.prefix_hash = hex::encode(Sha256::digest(wrapper));
    assert_eq!(
        NativeSubmitFloor::capture_before_submit(identity.execution(), &cursor).unwrap_err(),
        "native_identity_session_mismatch"
    );
}
