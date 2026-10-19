//! An executed resume resolves only the native resume Pending naming its exact descriptor.

use super::*;
use crate::services::claude_tui::hook_server::adoption_retry::DurableKind;
use std::io;

fn resume() -> Fixture {
    Fixture::new("resume", Some(ID))
}

fn write_rollout(h: &Fixture, path: &Path, id: &str, source: Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let at = (h.context.created_at - chrono::Duration::minutes(5)).to_rfc3339();
    let meta = json!({"type":"session_meta","timestamp":at,"payload":{"id":id,"timestamp":at,
        "cwd":h.root.path(),"source":source,"originator":"codex_cli_rs"}});
    fs::write(path, format!("{meta}\n")).unwrap();
}

/// A real hook delivery; `path: None` omits the payload transcript path.
fn hook(h: &Fixture, event: &str, id: &str, reason: &str, path: Option<&Path>) -> IngressOutcome {
    hook_with_source(h, event, id, Some(reason), path)
}

fn hook_with_source(
    h: &Fixture,
    event: &str,
    id: &str,
    reason: Option<&str>,
    path: Option<&Path>,
) -> IngressOutcome {
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(h.context.clone()),
        observed: ObservedHookProcess::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let mut payload = json!({"session_id":id,"prompt":PROMPT});
    if let Some(reason) = reason {
        payload["source"] = json!(reason);
    }
    if let Some(path) = path {
        payload["transcript_path"] = json!(path);
    }
    observe_binding_hook("codex", event, Some(id), Some(id), &payload, &headers)
}

fn record(h: &Fixture, id: &str, path: &Path) -> io::Result<bool> {
    codex_verified::record_executed_resume(&h.context, id, path, 0)
}

fn log_path(h: &Fixture) -> PathBuf {
    h.root
        .path()
        .join(binding_events::BINDING_EVENTS_DIR)
        .join("584504.log")
}

fn log_bytes(h: &Fixture) -> Vec<u8> {
    fs::read(log_path(h)).unwrap_or_default()
}

fn restart(h: &Fixture) {
    dedupe::reset_state_for_tests();
    binding_events::forget_channel_for_tests(584_504);
    register_tmux_channel(&h.context.tmux_session, 584_504);
}

/// A refused executed resume leaves the log, the Pending, and every runtime artifact untouched.
fn assert_blocked(h: &Fixture, result: io::Result<bool>, log: &[u8], pending: usize) {
    assert!(!matches!(result, Ok(true)), "{result:?}");
    assert_eq!(log_bytes(h), log, "refusal appends nothing");
    let fold = h.fold();
    assert!(fold.verified.is_none());
    assert_eq!(fold.pending.len(), pending);
    h.absent();
}

/// The hook-first log: native resume Pending, then the executed resume's Resolved.
fn hook_first(h: &Fixture) -> Vec<Value> {
    h.header(ID, false);
    assert_eq!(
        hook(h, "session-start", ID, "resume", Some(&h.path(ID))),
        IngressOutcome::Durable(DurableKind::Pending)
    );
    assert!(record(h, ID, &h.path(ID)).unwrap());
    fs::read_to_string(log_path(h))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn rewrite(h: &Fixture, rows: &[Value]) -> io::Result<binding_events::codex::Fold> {
    let text: String = rows.iter().map(|row| format!("{row}\n")).collect();
    fs::write(log_path(h), text).unwrap();
    binding_events::forget_channel_for_tests(584_504);
    let before = log_bytes(h);
    let fold = binding_events::codex::read_ownership(&h.context);
    assert_eq!(log_bytes(h), before, "replay is read-only");
    fold
}

#[test]
fn executed_resume_recovers_after_marker_before_append_restart() {
    let h = resume();
    h.header(ID, false);
    hook(&h, "session-start", ID, "resume", Some(&h.path(ID)));
    let pending = h.fold().pending[0].seq;
    restart(&h);
    codex_verified::resolve_registered_claims();
    let proof = h.fold().verified.expect("marker-backed executed resume");
    assert_eq!(proof.ownership.pending_seq, Some(pending));
    assert!(matches!(
        proof.ownership.claim.evidence,
        binding_events::codex::ClaimEvidence::ExplicitResume { .. }
    ));
    h.assert_proof();
}

#[test]
fn executed_resume_recovers_after_append_before_publication_restart() {
    let h = resume();
    h.header(ID, false);
    assert!(record(&h, ID, &h.path(ID)).unwrap());
    let (fold, log) = (h.fold(), log_bytes(&h));
    restart(&h);
    fs::remove_file(h.marker()).unwrap();
    codex_verified::resolve_registered_claims();
    assert_eq!(log_bytes(&h), log, "publication retry appends nothing");
    assert_eq!(h.fold(), fold);
    h.assert_proof();
}

#[test]
fn executed_resume_revalidation_rejects_descriptor_swap_without_append() {
    for in_place in [false, true] {
        let h = resume();
        h.header(ID, false);
        hook(&h, "session-start", ID, "resume", Some(&h.path(ID)));
        let log = log_bytes(&h);
        let path = h.path(ID);
        let root = h.root.path().to_path_buf();
        codex_verified::before_commit_for_tests(move || {
            if in_place {
                // Same inode, but no longer the CLI rollout the verifier opened.
                let at = chrono::Utc::now().to_rfc3339();
                let meta = json!({"type":"session_meta","timestamp":at,"payload":{"id":ID,"timestamp":at,"cwd":root,
                    "source":{"subagent":{"thread_spawn":{"parent_thread_id":CHILD}}},"originator":"codex_cli_rs"}});
                fs::write(&path, format!("{meta}\n")).unwrap();
            } else {
                fs::rename(&path, path.with_extension("held")).unwrap();
                fs::copy(path.with_extension("held"), &path).unwrap();
            }
        });
        let result = record(&h, ID, &h.path(ID));
        assert!(result.is_err(), "in_place={in_place}: {result:?}");
        assert_blocked(&h, result, &log, 1);
    }
}

#[test]
fn executed_resume_wrong_nonce_pending_stays_blocked() {
    let h = resume();
    h.header(ID, false);
    hook(&h, "session-start", ID, "resume", Some(&h.path(ID)));
    let log = log_bytes(&h);
    fs::write(
        crate::services::tmux_common::session_temp_path(&h.context.tmux_session, "spawn_nonce"),
        uuid::Uuid::new_v4().simple().to_string(),
    )
    .unwrap();
    let result = record(&h, ID, &h.path(ID));
    assert!(result.is_err());
    assert_blocked(&h, result, &log, 1);
}

#[test]
fn executed_resume_wrong_uuid_pending_stays_blocked() {
    let h = resume();
    h.header(ID, false);
    h.header(CHILD, false);
    hook(&h, "session-start", CHILD, "resume", None);
    let log = log_bytes(&h);
    let result = record(&h, ID, &h.path(ID));
    assert_blocked(&h, result, &log, 1);
    let result = record(&h, CHILD, &h.path(CHILD));
    assert!(
        result.is_err(),
        "an executed id other than the launch's own"
    );
    assert_blocked(&h, result, &log, 1);
}

#[test]
fn executed_resume_wrong_runtime_or_provider_root_stays_blocked() {
    let h = resume();
    let outside = h
        .root
        .path()
        .join("elsewhere")
        .join(format!("rollout-{ID}.jsonl"));
    write_rollout(&h, &outside, ID, json!("cli"));
    let result = record(&h, ID, &outside);
    assert!(result.is_err(), "source outside the context provider root");
    assert_blocked(&h, result, &[], 0);
    let mut foreign = h.context.clone();
    foreign.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
    let canonical = PreparedIncarnation::create(foreign.clone()).unwrap().path;
    // The same execution as published by another runtime's owner.
    foreign.owner_runtime_root = h.root.path().join("other-runtime").display().to_string();
    fs::write(canonical, serde_json::to_vec(&foreign).unwrap()).unwrap();
    fs::write(
        crate::services::tmux_common::session_temp_path(&h.context.tmux_session, "spawn_nonce"),
        &foreign.execution_nonce,
    )
    .unwrap();
    h.header(ID, false);
    let result = codex_verified::record_executed_resume(&foreign, ID, &h.path(ID), 0);
    assert!(
        result.is_err(),
        "the marker's execution is owned by another runtime"
    );
    assert!(log_bytes(&h).is_empty());
    assert!(
        !binding_events::codex::read_ownership(&foreign).is_ok_and(|fold| fold.verified.is_some())
    );
}

#[test]
fn executed_resume_different_descriptor_stays_blocked() {
    let h = resume();
    let sessions = h.context.provider_root.clone().unwrap();
    let (a, b) = (
        sessions.join("a").join(format!("rollout-{ID}.jsonl")),
        sessions.join("b").join(format!("rollout-{ID}.jsonl")),
    );
    write_rollout(&h, &a, ID, json!("cli"));
    write_rollout(&h, &b, ID, json!("cli"));
    hook(&h, "session-start", ID, "resume", Some(&b));
    let log = log_bytes(&h);
    let result = record(&h, ID, &a);
    assert_blocked(&h, result, &log, 1);
}

#[test]
fn executed_resume_pathless_ambiguous_uuid_stays_blocked() {
    let h = resume();
    let sessions = h.context.provider_root.clone().unwrap();
    let (a, b) = (
        sessions.join("a").join(format!("rollout-{ID}.jsonl")),
        sessions.join("b").join(format!("rollout-{ID}.jsonl")),
    );
    write_rollout(&h, &a, ID, json!("cli"));
    write_rollout(&h, &b, ID, json!("cli"));
    hook(&h, "session-start", ID, "resume", None);
    let log = log_bytes(&h);
    let result = record(&h, ID, &a);
    assert_blocked(&h, result, &log, 1);
}

#[test]
fn executed_resume_companion_ups_settles_before_initial_proof() {
    for pathless in [false, true] {
        for source in [None, Some("unknown"), Some("startup"), Some("resume")] {
            for ups_first in [false, true] {
                let h = resume();
                h.header(ID, false);
                let path = h.path(ID);
                let hint = (!pathless).then_some(path.as_path());
                let start = || hook(&h, "session-start", ID, "resume", hint);
                let ups = || hook_with_source(&h, "user-prompt-submit", ID, source, hint);
                let results = if ups_first {
                    [ups(), start()]
                } else {
                    [start(), ups()]
                };
                assert_eq!(
                    results,
                    [
                        IngressOutcome::Durable(DurableKind::Pending),
                        IngressOutcome::Durable(DurableKind::Pending)
                    ]
                );
                let pending = h.fold().pending;
                assert_eq!(pending.len(), 2);
                assert!(h.fold().verified.is_none());
                assert!(record(&h, ID, &path).unwrap());
                companions::assert_terminal_contract(&h, &pending);
            }
        }
    }
}

#[test]
fn executed_resume_other_lifecycle_pending_stays_blocked() {
    for (event, reason) in [
        ("session-start", "clear"),
        ("session-start", "compact"),
        ("session-start", "fork"),
        ("session-start", "startup"),
        ("user-prompt-submit", "startup"),
    ] {
        let h = resume();
        h.header(ID, false);
        hook(&h, event, ID, reason, Some(&h.path(ID)));
        let log = log_bytes(&h);
        assert_eq!(h.fold().pending.len(), 1, "{event}/{reason}");
        let result = record(&h, ID, &h.path(ID));
        assert_blocked(&h, result, &log, 1);
    }
}

#[path = "codex_verified_resume_companion_tests.rs"]
mod companions;

#[test]
fn executed_resume_nonempty_old_is_invalid() {
    let h = resume();
    let mut rows = hook_first(&h);
    let source = rows[1]["new"]["resolved"]["source"].clone();
    rows[1]["old"] = source;
    assert!(rewrite(&h, &rows).is_err());
}

#[test]
fn executed_resume_existing_verified_proof_cannot_use_initial_special_resolution() {
    let h = resume();
    let rows = hook_first(&h);
    let (pending, resolved) = (&rows[0], &rows[1]);
    let renumber = |row: &Value, seq: u64, pending_seq: Option<u64>| {
        let mut row = row.clone();
        row["seq"] = json!(seq);
        row["codex_ownership"]["seq"] = json!(seq);
        row["codex_ownership"]["pending_seq"] = json!(pending_seq);
        if let Some(pending_seq) = pending_seq {
            row["new"]["resolved"]["pending_seq"] = json!(pending_seq);
        }
        row
    };
    let mut source = renumber(resolved, 1, None);
    source["new"] = json!({"source": resolved["new"]["resolved"]["source"]});
    let rows = [
        source,
        renumber(pending, 2, None),
        renumber(resolved, 3, Some(2)),
    ];
    assert!(rewrite(&h, &rows).is_err());
}

#[test]
fn executed_resume_wrong_pending_seq_is_invalid() {
    let h = resume();
    let mut rows = hook_first(&h);
    rows[1]["codex_ownership"]["pending_seq"] = json!(7);
    rows[1]["new"]["resolved"]["pending_seq"] = json!(7);
    assert!(rewrite(&h, &rows).is_err());
}

#[test]
fn executed_resume_keeps_legacy_log_and_store_format() {
    let h = resume();
    let rows = hook_first(&h);
    // Only fields the existing schema-1 parser already knows.
    let known = [
        "schema",
        "seq",
        "context",
        "claim",
        "pending_seq",
        "published_at",
    ];
    for row in &rows {
        let ownership = row["codex_ownership"].as_object().unwrap();
        assert!(
            ownership.keys().all(|key| known.contains(&key.as_str())),
            "{row}"
        );
    }
    binding_events::forget_channel_for_tests(584_504);
    let events = binding_events::binding_events_since(584_504, 0).unwrap();
    assert!(matches!(
        events.iter().map(|event| &event.new).collect::<Vec<_>>()[..],
        [
            binding_events::BindingTarget::Pending { .. },
            binding_events::BindingTarget::Resolved { pending_seq: 1, .. }
        ]
    ));
    let fold = h.fold();
    let proof = fold.verified.unwrap();
    assert_eq!(proof.seq, 2);
    assert_eq!(
        binding_events::codex::proof_at_seq(&h.context, 2).unwrap(),
        Some(proof)
    );
    assert!(fold.pending.is_empty() && !fold.conflicted);
}
