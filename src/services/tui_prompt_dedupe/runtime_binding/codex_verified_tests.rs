#![cfg(unix)]

use super::codex_verified::{self, DeliveryPermission};
use super::*;
use crate::config::TestEnvVarGuard;
use crate::services::claude_tui::hook_server::observation_ingress::{
    IngressOutcome, observe_binding_hook,
};
use crate::services::tui_prompt_dedupe::{
    self as dedupe,
    binding_context::{
        BINDING_HEADER, BindingContext, CapturedContext, HookBindingEnvelope, ObservedHookProcess,
        PreparedIncarnation,
    },
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

const ID: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";
const CHILD: &str = "019e660d-4859-7522-9cee-8ba7c4e7c744";
const PROMPT: &str = "첫 argv\n한글 e\u{301}";

fn digest(s: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(s.as_bytes()))
}

struct Fixture {
    env: [TestEnvVarGuard; 2],
    root: tempfile::TempDir,
    context: BindingContext,
    canonical: PathBuf,
    _dedupe_lock: std::sync::MutexGuard<'static, ()>,
    _env_lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

impl Fixture {
    fn new(mode: &str, expected: Option<&str>) -> Self {
        Self::with_policy(mode, expected, Some("verified"))
    }
    fn with_policy(mode: &str, expected: Option<&str>, policy: Option<&str>) -> Self {
        let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let dedupe_lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (root, env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
        dedupe::reset_state_for_tests();
        binding_events::set_test_root(Some(root.path()));
        codex_verified::clear_permissions_for_tests();
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: format!("verified-{}", uuid::Uuid::new_v4().simple()),
            channel_id: Some(584_504),
            owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: expected.map(str::to_owned),
            launch_mode: mode.into(),
            provider_root: Some(sessions.canonicalize().unwrap()),
            first_prompt_digest: Some(digest(PROMPT)),
            source_policy: policy.map(str::to_owned),
        };
        let prepared = PreparedIncarnation::create(context.clone()).unwrap();
        let marker =
            crate::services::tmux_common::session_temp_path(&context.tmux_session, "spawn_nonce");
        fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        fs::write(marker, &context.execution_nonce).unwrap();
        register_tmux_channel(&context.tmux_session, 584_504);
        codex_verified::set_permission_for_tests(&context, DeliveryPermission::Allowed);
        Self {
            env,
            root,
            context,
            canonical: prepared.path,
            _dedupe_lock: dedupe_lock,
            _env_lock: env_lock,
        }
    }
    fn path(&self, id: &str) -> PathBuf {
        self.context
            .provider_root
            .as_ref()
            .unwrap()
            .join(format!("rollout-{id}.jsonl"))
    }
    fn header(&self, id: &str, child: bool) {
        let source = if child {
            json!({"subagent":{"thread_spawn":{"parent_thread_id":ID}}})
        } else {
            json!("cli")
        };
        let meta = json!({"type":"session_meta", "timestamp":(self.context.created_at + chrono::Duration::seconds(1)).to_rfc3339(), "payload":{"id":id,"timestamp":(self.context.created_at + chrono::Duration::seconds(1)).to_rfc3339(),"cwd":self.root.path(),"source":source,"originator":"codex_cli_rs"}});
        fs::write(self.path(id), format!("{meta}\n")).unwrap();
    }
    fn send(
        &self,
        event: &str,
        id: &str,
        prompt: Value,
        captured: Option<BindingContext>,
    ) -> IngressOutcome {
        let envelope = HookBindingEnvelope {
            context: CapturedContext::Captured(captured.unwrap_or_else(|| self.context.clone())),
            observed: ObservedHookProcess::default(),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
        let mut payload =
            json!({"session_id":id,"transcript_path":self.path(id),"source":"startup"});
        if prompt != json!({"test_missing_prompt":true}) {
            payload["prompt"] = prompt;
        }
        observe_binding_hook("codex", event, Some(id), Some(id), &payload, &headers)
    }
    fn fold(&self) -> binding_events::codex::Fold {
        binding_events::codex::read_ownership(&self.context).unwrap()
    }
    fn raw(&self) -> Option<TuiRuntimeBinding> {
        STATE
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .runtime_by_tmux
            .get(&self.context.tmux_session)
            .map(|b| b.value.clone())
    }
    fn marker(&self) -> PathBuf {
        PathBuf::from(crate::services::tmux_common::session_temp_path(
            &self.context.tmux_session,
            crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        ))
    }
    fn absent(&self) {
        assert!(self.raw().is_none());
        assert!(!self.marker().exists());
    }
    fn consumer(&self) -> Option<TuiRuntimeBinding> {
        runtime_binding_for_tmux_session(&self.context.tmux_session)
    }
    fn assert_proof(&self) {
        assert!(self.fold().verified.is_some());
        assert!(self.consumer().is_some());
        assert!(self.marker().is_file());
    }
    fn binding(&self) -> TuiRuntimeBinding {
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: self.path(ID).display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(ID.into()),
            last_offset: 0,
            relay_last_offset: Some(0),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        binding_events::APPEND_FAULT.with(|f| f.set(None));
        codex_verified::clear_permissions_for_tests();
        binding_events::forget_channel_for_tests(584_504);
        binding_events::set_test_root(None);
        dedupe::reset_state_for_tests();
        let _ = &self.env;
    }
}

#[test]
fn verified_source_less_equal_command_initial_ups_uses_exact_utf8() {
    let h = Fixture::new("fresh", None);
    h.header(ID, false);
    let outcome = h.send("user-prompt-submit", ID, json!(PROMPT), None);
    assert!(!outcome.refused(), "{outcome:?}");
    h.assert_proof();
    let before = h.fold();
    h.send("user-prompt-submit", ID, json!(PROMPT), None);
    assert_eq!(h.fold(), before, "same claim retry must not append");
}

#[test]
fn actual_initial_ups_missing_nonstring_mismatch_never_proves() {
    for prompt in [
        json!({"test_missing_prompt":true}),
        Value::Null,
        json!(9),
        json!({"text":PROMPT}),
        json!("different"),
        json!("첫 argv\n한글 é"),
    ] {
        let h = Fixture::new("fresh", None);
        h.header(ID, false);
        let outcome = h.send("user-prompt-submit", ID, prompt, None);
        assert!(
            !outcome.refused(),
            "ineligible UPS is durable Pending: {outcome:?}"
        );
        assert!(h.fold().verified.is_none());
        assert!(!h.fold().pending.is_empty());
        h.absent();
    }
}

#[test]
fn actual_ups_fresh_qualification_cannot_be_replaced_by_matching_digest() {
    for (mode, expected) in [("resume", Some(CHILD)), ("fresh", Some(CHILD))] {
        let h = Fixture::new(mode, expected);
        h.header(ID, false);
        h.send("user-prompt-submit", ID, json!(PROMPT), None);
        assert!(h.fold().verified.is_none(), "{mode}/{expected:?}");
        h.absent();
    }
}

#[test]
fn pending_ack_resolves_without_a_followup_hook() {
    let h = Fixture::new("fresh", None);
    let outcome = h.send("session-start", ID, Value::Null, None);
    assert!(!outcome.refused(), "{outcome:?}");
    assert_eq!(h.fold().pending.len(), 1);
    h.absent();
    h.header(ID, false);
    codex_verified::resolve_registered_claims();
    h.assert_proof();
    let before = h.fold();
    codex_verified::resolve_registered_claims();
    assert_eq!(h.fold(), before);
}

#[test]
fn child_rejection_retries_remaining_parent_in_the_same_resolver_pass() {
    for child_first in [true, false] {
        let h = Fixture::new("fresh", None);
        for id in if child_first {
            [CHILD, ID]
        } else {
            [ID, CHILD]
        } {
            h.send("session-start", id, Value::Null, None);
        }
        assert_eq!(h.fold().pending.len(), 2);
        h.header(ID, false);
        h.header(CHILD, true);
        codex_verified::resolve_registered_claims();
        h.assert_proof();
        assert!(h.fold().pending.is_empty());
        assert_eq!(h.consumer().unwrap().session_id.as_deref(), Some(ID));
    }
}

#[test]
fn cancelled_and_unknown_permission_preserve_proof_but_publish_nothing() {
    for permission in [DeliveryPermission::Cancelled, DeliveryPermission::Unknown] {
        let h = Fixture::new("fresh", None);
        h.header(ID, false);
        codex_verified::set_permission_for_tests(&h.context, permission);
        let outcome = h.send("session-start", ID, Value::Null, None);
        assert_eq!(
            outcome,
            IngressOutcome::Durable(
                crate::services::claude_tui::hook_server::adoption_retry::DurableKind::Pending
            )
        );
        assert!(h.fold().verified.is_some());
        h.absent();
        STATE
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .runtime_by_tmux
            .insert(
                h.context.tmux_session.clone(),
                TimedValue {
                    value: h.binding(),
                    recorded_at: Instant::now(),
                },
            );
        assert!(h.consumer().is_none());
        crate::services::tmux_common::with_tmux_source_authority(&h.context.tmux_session, |a| {
            assert!(runtime_binding_for_tmux_session_under_source_authority(a).is_none())
        });
    }
}

#[test]
fn canonical_mismatch_and_stale_stamp_do_not_open_or_record_native_claim() {
    for stale in [false, true] {
        let h = Fixture::new("fresh", None);
        h.header(ID, false);
        let mut captured = h.context.clone();
        if stale {
            fs::write(
                crate::services::tmux_common::session_temp_path(
                    &h.context.tmux_session,
                    "spawn_nonce",
                ),
                "b".repeat(32),
            )
            .unwrap();
        } else {
            captured.first_prompt_digest = Some(digest("forged"));
        }
        h.send("session-start", ID, Value::Null, Some(captured));
        assert!(h.fold().verified.is_none());
        assert!(h.fold().pending.is_empty());
        h.absent();
        assert_eq!(
            fs::read(h.canonical.clone()).unwrap(),
            serde_json::to_vec(&h.context).unwrap()
        );
    }
}

#[test]
fn append_failure_cannot_publish_proof_runtime_or_marker() {
    for phase in ["write", "sync"] {
        let h = Fixture::new("fresh", None);
        h.header(ID, false);
        binding_events::APPEND_FAULT.with(|f| f.set(Some(phase)));
        let outcome = h.send("session-start", ID, Value::Null, None);
        binding_events::APPEND_FAULT.with(|f| f.set(None));
        assert!(outcome.refused(), "{phase}: {outcome:?}");
        assert!(h.fold().verified.is_none());
        h.absent();
    }
}

#[test]
fn marker_failure_retains_durable_proof_and_retry_does_not_append() {
    let h = Fixture::new("fresh", None);
    h.header(ID, false);
    fs::create_dir_all(h.marker()).unwrap();
    let outcome = h.send("session-start", ID, Value::Null, None);
    assert!(outcome.refused(), "{outcome:?}");
    assert!(h.fold().verified.is_some());
    assert!(h.raw().is_none());
    let before = h.fold();
    fs::remove_dir(h.marker()).unwrap();
    codex_verified::resolve_registered_claims();
    h.assert_proof();
    assert_eq!(h.fold(), before);
}

#[test]
fn generic_registration_before_and_between_hooks_never_creates_ownership() {
    for generic_first in [false, true] {
        let h = Fixture::new("fresh", None);
        h.header(ID, false);
        if !generic_first {
            h.send("session-start", ID, Value::Null, None);
            h.assert_proof();
        }
        register_tmux_runtime_binding(&h.context.tmux_session, h.binding());
        register_rehydrated_tmux_runtime_binding(
            "codex",
            &h.context.tmux_session,
            584_504,
            h.binding(),
        );
        if generic_first {
            h.absent();
            assert!(h.fold().verified.is_none());
            h.send("session-start", ID, Value::Null, None);
        }
        h.send("user-prompt-submit", ID, json!(PROMPT), None);
        h.assert_proof();
    }
}

#[test]
fn concurrent_start_and_matching_ups_share_one_native_proof() {
    let h = Fixture::new("fresh", None);
    h.header(ID, false);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let context = h.context.clone();
    let path = h.path(ID);
    let root = h.root.path().to_path_buf();
    std::thread::scope(|scope| {
        for event in ["session-start", "user-prompt-submit"] {
            let context = context.clone();
            let path = path.clone();
            let barrier = barrier.clone();
            let root = root.clone();
            scope.spawn(move || {
                binding_events::set_test_root(Some(&root));
                codex_verified::set_permission_for_tests(&context,DeliveryPermission::Allowed);
                let envelope = HookBindingEnvelope { context:CapturedContext::Captured(context),observed:ObservedHookProcess::default() };
                let mut headers = axum::http::HeaderMap::new();
                headers.insert(BINDING_HEADER,envelope.encode().unwrap().parse().unwrap());
                let payload = json!({"session_id":ID,"transcript_path":path,"source":"startup","prompt":PROMPT});
                barrier.wait();
                let outcome = observe_binding_hook("codex",event,Some(ID),Some(ID),&payload,&headers);
                assert!(!outcome.refused(),"{event}: {outcome:?}");
                codex_verified::clear_permissions_for_tests();
                binding_events::set_test_root(None);
            });
        }
    });
    codex_verified::resolve_registered_claims();
    h.assert_proof();
    assert!(h.fold().pending.is_empty());
    let before = h.fold();
    h.send("session-start", ID, Value::Null, None);
    assert_eq!(h.fold(), before);
}

#[test]
fn published_proof_later_ups_and_discovery_preserve_delivery_cursors() {
    let h = Fixture::new("fresh", None);
    h.header(ID, false);
    h.send("session-start", ID, Value::Null, None);
    h.assert_proof();
    let before = h.fold();
    let mut discovered = h.binding();
    discovered.last_offset = u64::MAX;
    discovered.relay_last_offset = Some(u64::MAX);
    discovered.relay_output_path = Some(
        h.root
            .path()
            .join("different-relay.jsonl")
            .display()
            .to_string(),
    );
    register_tmux_runtime_binding(&h.context.tmux_session, discovered.clone());
    assert_eq!(h.consumer().unwrap().last_offset, 0);
    crate::services::codex_tui::session::advance_codex_tui_runtime_binding_and_marker_offset(
        &h.context.tmux_session,
        &h.path(ID),
        12,
    );
    register_rehydrated_tmux_runtime_binding("codex", &h.context.tmux_session, 584_504, discovered);
    h.send(
        "user-prompt-submit",
        ID,
        json!("subsequent different prompt"),
        None,
    );
    assert_eq!(h.fold(), before);
    let binding = h.consumer().unwrap();
    assert_eq!(binding.last_offset, 12);
    assert_eq!(binding.relay_last_offset(), 12);
    assert_eq!(binding.relay_output_path, None);
}

#[test]
fn revoked_permission_blocks_both_getters_with_an_intact_proof_and_marker() {
    for permission in [DeliveryPermission::Unknown, DeliveryPermission::Cancelled] {
        let h = Fixture::new("fresh", None);
        h.header(ID, false);
        h.send("session-start", ID, Value::Null, None);
        h.assert_proof();
        let marker = fs::read(h.marker()).unwrap();
        let before = h.fold();
        codex_verified::set_permission_for_tests(&h.context, permission);
        assert!(h.consumer().is_none());
        crate::services::tmux_common::with_tmux_source_authority(&h.context.tmux_session, |a| {
            assert!(runtime_binding_for_tmux_session_under_source_authority(a).is_none());
            assert!(!codex_verified::publication_allowed(a, &h.binding()));
        });
        assert!(runtime_bindings_for_kind(RuntimeHandoffKind::CodexTui).is_empty());
        assert!(
            crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset(
                &h.context.tmux_session,
                &h.path(ID),
                Some(ID),
                Some(u64::MAX)
            )
            .is_err()
        );
        assert_eq!(fs::read(h.marker()).unwrap(), marker);
        assert_eq!(h.fold(), before);
        assert!(h.raw().is_some());
    }
}

#[test]
fn cached_generic_binding_and_forged_marker_cannot_satisfy_a_getter() {
    let h = Fixture::new("fresh", None);
    h.header(ID, false);
    STATE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .runtime_by_tmux
        .insert(
            h.context.tmux_session.clone(),
            TimedValue {
                value: TuiRuntimeBinding {
                    last_offset: u64::MAX,
                    relay_last_offset: Some(u64::MAX),
                    ..h.binding()
                },
                recorded_at: Instant::now(),
            },
        );
    fs::write(
        h.marker(),
        serde_json::to_vec(
            &json!({"rollout_path":h.path(ID), "session_id":ID, "rollout_start_offset":u64::MAX}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(h.consumer().is_none());
    assert!(runtime_bindings_for_kind(RuntimeHandoffKind::CodexTui).is_empty());
    h.send("session-start", ID, Value::Null, None);
    h.assert_proof();
    assert_eq!(h.consumer().unwrap().last_offset, 0);
    let mut marker: Value = serde_json::from_slice(&fs::read(h.marker()).unwrap()).unwrap();
    assert_eq!(marker["rollout_start_offset"], json!(0));
    marker["codex_ownership"]["proof_seq"] = json!(u64::MAX);
    fs::write(h.marker(), serde_json::to_vec(&marker).unwrap()).unwrap();
    assert!(h.consumer().is_none());
}

#[test]
fn unresolved_two_native_parents_and_stop_do_not_publish_a_winner() {
    let h = Fixture::new("fresh", None);
    h.send("session-start", ID, Value::Null, None);
    h.send("session-start", CHILD, Value::Null, None);
    h.header(ID, false);
    h.header(CHILD, false);
    codex_verified::resolve_registered_claims();
    assert!(h.fold().verified.is_none());
    assert_eq!(h.fold().pending.len(), 2);
    h.send("stop", ID, Value::Null, None);
    assert!(h.fold().verified.is_none());
    h.absent();
}

#[test]
fn canonical_corruption_after_claim_never_falls_back_to_generic_cache() {
    let h = Fixture::new("fresh", None);
    h.header(ID, false);
    h.send("session-start", ID, Value::Null, None);
    h.assert_proof();
    fs::write(&h.canonical, b"corrupt").unwrap();
    assert!(h.consumer().is_none());
    codex_verified::resolve_registered_claims();
    assert!(h.consumer().is_none());
}

#[test]
fn actual_ingress_revalidates_incarnation_inside_the_log_commit() {
    let h = Fixture::new("fresh", None);
    h.header(ID, false);
    let marker =
        crate::services::tmux_common::session_temp_path(&h.context.tmux_session, "spawn_nonce");
    codex_verified::before_commit_for_tests(move || {
        fs::write(marker, "b".repeat(32)).unwrap();
    });
    let outcome = h.send("session-start", ID, Value::Null, None);
    assert!(outcome.refused());
    assert!(h.fold().verified.is_none());
    assert!(h.fold().pending.is_empty());
    h.absent();
}

#[test]
fn actual_marker_and_launch_install_before_claim_are_refused() {
    let h = Fixture::new("fresh", None);
    h.header(ID, false);
    assert!(
        crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset(
            &h.context.tmux_session,
            &h.path(ID),
            Some(ID),
            Some(u64::MAX)
        )
        .is_err()
    );
    assert!(
        !crate::services::codex_tui::session::install_launched_codex_tui_runtime_binding(
            &h.context.tmux_session,
            Some(u64::MAX),
            h.binding()
        )
    );
    h.absent();
    assert!(h.fold().verified.is_none());
    h.send("session-start", ID, Value::Null, None);
    h.assert_proof();
}

#[test]
fn missing_and_unknown_nonce_do_not_export_an_older_proof() {
    for nonce in [None, Some("b".repeat(32))] {
        let h = Fixture::new("fresh", None);
        h.header(ID, false);
        h.send("session-start", ID, Value::Null, None);
        h.assert_proof();
        let marker =
            crate::services::tmux_common::session_temp_path(&h.context.tmux_session, "spawn_nonce");
        if let Some(nonce) = nonce {
            fs::write(marker, nonce).unwrap();
        } else {
            fs::remove_file(marker).unwrap();
        }
        assert!(h.consumer().is_none());
        assert!(runtime_bindings_for_kind(RuntimeHandoffKind::CodexTui).is_empty());
        codex_verified::resolve_registered_claims();
        assert!(h.consumer().is_none());
    }
}

#[test]
fn nonverified_incarnations_keep_legacy_publication_independent_of_process_mode() {
    use crate::services::codex::{CodexSourceMode, SOURCE_MODE_TEST};
    for policy in [None, Some("legacy"), Some("shadow")] {
        for mode in [
            CodexSourceMode::Legacy,
            CodexSourceMode::Shadow,
            CodexSourceMode::Verified,
        ] {
            let h = Fixture::with_policy("fresh", None, policy);
            h.header(ID, false);
            SOURCE_MODE_TEST.with(|m| m.set(Some(mode)));
            let result = h.send("user-prompt-submit", ID, json!(PROMPT), None);
            SOURCE_MODE_TEST.with(|m| m.set(None));
            assert!(!result.refused(), "{policy:?}/{mode:?}: {result:?}");
            register_tmux_runtime_binding(&h.context.tmux_session, h.binding());
            assert!(h.consumer().is_some());
            let log = fs::read_to_string(
                h.root
                    .path()
                    .join(binding_events::BINDING_EVENTS_DIR)
                    .join("584504.log"),
            )
            .unwrap_or_default();
            for line in log.lines() {
                let record: Value = serde_json::from_str(line).unwrap();
                assert!(record.get("codex_ownership").is_none());
            }
        }
    }
    assert_eq!(
        CodexSourceMode::Verified.launch_policy(),
        Err("SourceModeVerifiedNotLanded")
    );
}

#[test]
fn actual_commit_rechecks_open_file_identity_and_startup_recovers_bad_initial_ups() {
    {
        let h = Fixture::new("fresh", None);
        h.header(ID, false);
        let path = h.path(ID);
        codex_verified::before_commit_for_tests(move || {
            let bytes = fs::read(&path).unwrap();
            fs::rename(&path, path.with_extension("old")).unwrap();
            fs::write(path, bytes).unwrap();
        });
        assert!(h.send("session-start", ID, Value::Null, None).refused());
        assert!(h.fold().verified.is_none());
        h.absent();
    }
    let h = Fixture::new("fresh", None);
    h.header(ID, false);
    h.send("user-prompt-submit", ID, json!("not initial prompt"), None);
    h.absent();
    h.send("session-start", ID, Value::Null, None);
    h.assert_proof();
    assert!(h.fold().pending.is_empty());
}

#[test]
fn production_rollout_selector_still_picks_newest_mtime_even_with_an_owned_source() {
    let h = Fixture::new("fresh", None);
    h.header(ID, false);
    h.send("session-start", ID, Value::Null, None);
    h.assert_proof();
    h.header(CHILD, false);
    for (id, seconds) in [(ID, 10), (CHILD, 20)] {
        fs::File::options()
            .write(true)
            .open(h.path(id))
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds))
            .unwrap();
    }
    let selected = crate::services::codex_tui::rollout_tail::latest_rollout_for_cwd_since(
        h.root.path(),
        std::time::UNIX_EPOCH,
        h.context.provider_root.as_ref().unwrap(),
    );
    assert_eq!(selected, Some(h.path(CHILD)));
    assert_eq!(h.consumer().unwrap().session_id.as_deref(), Some(ID));
}
