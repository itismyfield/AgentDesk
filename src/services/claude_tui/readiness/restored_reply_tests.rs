#![cfg(unix)]
use super::*;
use crate::services::tui_prompt_dedupe::binding_events;
#[cfg(test)]
use crate::services::tui_prompt_dedupe::binding_events::{
    BindingCause, CauseSource, HookSignal, Proposal, SourceId,
};
#[cfg(test)]
use crate::services::tui_turn_state::idle_compaction_tests::{end_turn, tail};
#[cfg(test)]
use std::os::unix::fs::MetadataExt;

#[cfg(test)]
fn completed(r: &Rig, at: DateTime<Utc>) {
    let mut reply = end_turn();
    reply["timestamp"] = json!(at);
    r.append(reply);
    for record in tail() {
        r.append(record);
    }
}
struct Pinned {
    rig: Rig,
    previous_root: Option<PathBuf>,
}
impl std::ops::Deref for Pinned {
    type Target = Rig;
    fn deref(&self) -> &Rig {
        &self.rig
    }
}
impl std::ops::DerefMut for Pinned {
    fn deref_mut(&mut self) -> &mut Rig {
        &mut self.rig
    }
}
impl Drop for Pinned {
    fn drop(&mut self) {
        binding_events::set_test_root(self.previous_root.as_deref());
    }
}
#[cfg(test)]
fn pinned() -> Pinned {
    let r = Rig::launched(Some(8_222_001), Utc::now());
    let previous_root = binding_events::test_root();
    binding_events::set_test_root(Some(r._dir.path()));
    completed(&r, r.ctx.created_at);
    let meta = fs::metadata(&r.path).unwrap();
    let id = SourceId {
        session_id: r.ctx.expected_native_session_id.clone().unwrap(),
        path: r.path.clone(),
        dev: meta.dev(),
        ino: meta.ino(),
    };
    let text = r.path.to_str().unwrap();
    let hook = HookSignal::from_payload(
        "session_start",
        &json!({"source":"startup", "transcript_path":text}),
    );
    let proposed = Proposal {
        channel_id: 8_222_001,
        provider: "claude",
        tmux_session: &r.session,
        session_id: Some(&id.session_id),
        path: text,
        replaced: None,
        cause: CauseSource::Hook(BindingCause::Startup),
        hook: Some(&hook),
    };
    binding_events::record_verified(&proposed, &id).unwrap();
    let (_, history) =
        binding_events::claude_history(8_222_001, &r.session, Some(&r.ctx.execution_nonce))
            .unwrap();
    assert!(history.complete);
    assert_eq!(history.current.unwrap().pin, Some(id));
    Pinned {
        rig: r,
        previous_root,
    }
}
#[cfg(test)]
fn candidate(r: &Rig) -> Option<Proof> {
    observe_restored_reply(&r.session, Some(&r.path))
}
#[cfg(test)]
fn save_context(r: &Rig) {
    let path = crate::services::tui_prompt_dedupe::binding_context::context_path(
        "claude",
        &r.ctx.execution_nonce,
    )
    .unwrap();
    fs::write(path, serde_json::to_vec(&r.ctx).unwrap()).unwrap();
}

#[cfg(test)]
fn repin(r: &Rig, native: &str, path: &Path) {
    let text = path.to_str().unwrap();
    let hook = HookSignal::from_payload(
        "session_start",
        &json!({"source":"clear","session_id":native,"transcript_path":text}),
    );
    let meta = fs::metadata(path).unwrap();
    let id = SourceId {
        session_id: native.into(),
        path: path.into(),
        dev: meta.dev(),
        ino: meta.ino(),
    };
    binding_events::record_verified(
        &Proposal {
            channel_id: 8_222_001,
            provider: "claude",
            tmux_session: &r.session,
            session_id: Some(native),
            path: text,
            replaced: None,
            cause: CauseSource::Hook(BindingCause::Clear),
            hook: Some(&hook),
        },
        &id,
    )
    .unwrap();
    assert_eq!(
        binding_events::pinned_source(8_222_001, &r.session).unwrap(),
        Some(id)
    );
}

#[test]
fn restored_reply_recovers_without_start_or_new_activity_and_reserves_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = pinned();
    assert!(!r.ready());
    let proof = candidate(&r).expect("the current launch's pinned reply recovers its lost proof");
    assert!(proof.origin == ProofOrigin::RestoredReply && proof.restart());
    assert!(
        !r.ready(),
        "restoration never clears the shared restart floor"
    );
    let reservation = reserve(proof).unwrap();
    assert!(reservation.current());
    assert!(candidate(&r).is_none());
    reservation.not_sent();
    assert!(candidate(&r).is_some());
}
#[test]
fn restored_reply_nonce_guard_rejects_another_incarnations_global_pin() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = pinned();
    let mut ctx = r.ctx.clone();
    ctx.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
    crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(ctx.clone())
        .unwrap();
    fs::write(
        crate::services::tmux_common::session_temp_path(&r.session, "spawn_nonce"),
        &ctx.execution_nonce,
    )
    .unwrap();
    assert!(
        candidate(&r).is_none(),
        "another nonce cannot borrow the old pinned reply"
    );
}
#[test]
fn restored_reply_native_guard_rejects_a_replaced_launch_session() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = pinned();
    repin(&r, &uuid::Uuid::new_v4().to_string(), &r.path);
    assert!(
        candidate(&r).is_none(),
        "the current pin and runtime must name the same native"
    );
}

#[test]
fn restored_reply_inode_guard_rejects_a_replacement_before_the_first_read() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = pinned();
    let copy = r.path.with_extension("copy");
    fs::copy(&r.path, &copy).unwrap();
    fs::rename(copy, &r.path).unwrap();
    assert!(
        candidate(&r).is_none(),
        "identical bytes in another inode are not the pinned source"
    );
}
#[test]
fn restored_reply_stop_guard_rejects_an_unclassified_stop_success() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = pinned();
    r.append(
        json!({"type":"attachment","attachment":{"type":"hook_success", "hookEvent":"Stop",
        "toolUseID":"unknown-stop", "stdout":"{}"}}),
    );
    assert!(
        candidate(&r).is_none(),
        "unknown Stop output does not prove absence of added context"
    );
}
#[test]
fn restored_reply_launch_guard_rejects_an_earlier_runs_reply() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = pinned();
    fs::write(&r.path, "").unwrap();
    completed(&r, r.ctx.created_at - chrono::Duration::nanoseconds(1));
    assert!(
        candidate(&r).is_none(),
        "old native history is not a reply of this launch"
    );
}
#[test]
fn restored_reply_never_accepts_a_composer_only_hard_restoration() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    initialize();
    let r = Rig::launched(None, *STARTED - chrono::Duration::hours(1));
    r.assistant("an old completed answer");
    r.terminal();
    let hard = observe_restored(&r.session, Some(&r.path)).unwrap();
    assert!(hard.origin == ProofOrigin::RestoredHard && hard.restart());
    assert!(
        candidate(&r).is_none(),
        "Busy's new API never admits RestoredHard"
    );
    let reservation = reserve(hard).unwrap();
    assert!(reservation.current());
    reservation.not_sent();
}
#[test]
fn restored_reply_rechecks_pin_native_and_stop_before_reserving() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for change in ["nonce", "native", "inode", "stop"] {
        let r = pinned();
        let proof = candidate(&r).unwrap();
        match change {
            "nonce" => { let mut ctx = r.ctx.clone(); ctx.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
                crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(ctx.clone()).unwrap();
                fs::write(crate::services::tmux_common::session_temp_path(&r.session,"spawn_nonce"), &ctx.execution_nonce).unwrap(); }
            "native" => repin(&r, &uuid::Uuid::new_v4().to_string(), &r.path),
            "inode" => { let copy = r.path.with_extension("copy"); fs::copy(&r.path, &copy).unwrap(); fs::rename(copy, &r.path).unwrap(); }
            _ => r.append(json!({"type":"attachment","attachment":{"type":"hook_success","hookEvent":"Stop","stdout":"{}"}})),
        }
        assert!(
            reserve(proof).is_none(),
            "reserve must repeat the same origin's {change} check"
        );
    }
}
#[test]
fn restored_reply_reservation_rechecks_a_native_change_without_new_bytes() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = pinned();
    let reservation = reserve(candidate(&r).unwrap()).unwrap();
    repin(&r, &uuid::Uuid::new_v4().to_string(), &r.path);
    assert!(!reservation.current());
    reservation.not_sent();
}
#[test]
fn restored_reply_refuses_new_prompts_tools_stop_context_and_unreadable_history() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for next in [
        json!({"type":"user","message":{"role":"user","content":"next"}}),
        json!({"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"t","name":"Read","input":{}}]}}),
        json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"done"}]}}),
        json!({"type":"attachment","attachment":{"type":"hook_success","hookEvent":"Stop","stdout":"{\"hookSpecificOutput\":{\"hookEventName\":\"Stop\",\"additionalContext\":\"continue\"}}"}}),
        json!({"type":"system","subtype":"stop_hook_summary","preventedContinuation":true,"hookAdditionalContext":["continue"]}),
        json!({"type":"attachment","attachment":{"type":"hook_success","hookEvent":"UserPromptSubmit"}}),
        json!({"type":"attachment","attachment":{"type":"hook_success","hookName":"PreToolUse"}}),
        json!({"type":"attachment","attachment":{"type":"hook_success","hookName":"PostToolUse"}}),
        json!({"type":"attachment","attachment":{"type":"hook_success","stdout":"{\"decision\":\"block\",\"reason\":\"continue\"}"}}),
        json!({"type":"future-record"}),
    ] {
        let r = pinned();
        r.append(next.clone());
        assert!(candidate(&r).is_none(), "{next}");
    }
    let r = pinned();
    write!(
        OpenOptions::new().append(true).open(&r.path).unwrap(),
        "{{\"type\":"
    )
    .unwrap();
    assert!(candidate(&r).is_none());
}
#[test]
fn restored_reply_respects_continuation_and_compact_fences() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = pinned();
    note_hook(
        "claude",
        &HookEventKind::Stop,
        &r.session,
        &json!({"session_id":r.ctx.expected_native_session_id,"transcript_path":r.path}),
        Some(&r.ctx),
        Some(Utc::now()),
        true,
    );
    assert!(candidate(&r).is_none());
    let r = pinned();
    assert!(candidate(&r).is_some());
    let source = Source::current(&r.session, Some(&r.path)).unwrap();
    ledger().entries.entry(source).or_default().compact_ticket = Some(1);
    assert!(candidate(&r).is_none());
}

#[test]
fn restored_reply_accepts_only_a_known_quiet_relay_stop() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = pinned();
    let native = r.ctx.expected_native_session_id.as_ref().unwrap();
    r.append(json!({"type":"attachment","attachment":{"type":"hook_success", "hookEvent":"Stop", "hookName":"Stop",
        "command":format!("node claude-hook-relay.mjs --session-id {native}"), "stdout":"{\"suppressOutput\":true}"}}));
    assert!(candidate(&r).is_some());
}

#[test]
fn restored_reply_reservation_cannot_substitute_a_different_proof_origin() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = pinned();
    let proof = candidate(&r).unwrap();
    ledger().entries.get_mut(&proof.source).unwrap().recovery = None;
    assert!(
        reserve(proof).is_none(),
        "same source and stamp do not substitute another origin"
    );
}

#[test]
fn restored_reply_refuses_missing_launch_fields_and_future_timestamps() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for case in ["future", "timestamp"] {
        let r = pinned();
        fs::write(&r.path, "").unwrap();
        if case == "future" {
            completed(&r, Utc::now() + chrono::Duration::days(1));
        } else {
            r.append(end_turn());
            for record in tail() {
                r.append(record);
            }
        }
        assert!(candidate(&r).is_none(), "{case}");
    }
    let r = Rig::launched(Some(8_222_001), Utc::now());
    completed(&r, r.ctx.created_at);
    assert!(
        candidate(&r).is_none(),
        "a source without a verified pin is never restored"
    );
}

#[test]
fn restored_reply_tracks_the_current_native_after_clear_in_the_same_execution() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut r = pinned();
    let initial = r.ctx.expected_native_session_id.clone();
    let native = uuid::Uuid::new_v4().to_string();
    let path = r.path.with_file_name(format!("{native}.jsonl"));
    let mut reply = end_turn();
    reply["sessionId"] = json!(native);
    reply["timestamp"] = json!(Utc::now());
    let mut records = vec![reply];
    records.extend(tail());
    let text = records
        .into_iter()
        .map(|mut record| {
            record["sessionId"] = json!(native);
            record.to_string() + "\n"
        })
        .collect::<String>();
    fs::write(&path, text).unwrap();
    repin(&r, &native, &path);
    let mut binding = tui_prompt_dedupe::runtime_binding_for_tmux_session(&r.session).unwrap();
    binding.session_id = Some(native);
    binding.output_path = path.to_string_lossy().into_owned();
    tui_prompt_dedupe::register_tmux_runtime_binding(&r.session, binding);
    assert_eq!(r.ctx.expected_native_session_id, initial);
    assert!(observe_restored_reply(&r.session, Some(&path)).is_some());
    r.ctx.expected_native_session_id = None;
    save_context(&r);
    assert!(
        observe_restored_reply(&r.session, Some(&path)).is_some(),
        "an optional launch hint cannot veto the verified current native"
    );
}
