//! Executed resume companions use durable exact-sequence terminals and recovery.

use super::*;
use std::os::unix::fs::MetadataExt;

fn rows(h: &Fixture) -> Vec<Value> {
    fs::read_to_string(log_path(h))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

pub(super) fn assert_terminal_contract(
    h: &Fixture,
    pending: &[binding_events::codex::ClaimRecord],
) {
    let fold = h.fold();
    assert!(fold.pending.is_empty() && !fold.conflicted);
    let proof = fold.verified.unwrap();
    let path = h.path(ID).canonicalize().unwrap();
    let metadata = fs::metadata(&path).unwrap();
    assert_eq!(
        (
            proof.source.session_id.as_str(),
            &proof.source.path,
            proof.source.dev,
            proof.source.ino
        ),
        (ID, &path, metadata.dev(), metadata.ino())
    );
    assert!(matches!(proof.ownership.claim.evidence,
        binding_events::codex::ClaimEvidence::ExplicitResume { ref executed_session_id } if executed_session_id == ID));
    let rows = rows(h);
    for p in pending {
        let terminal: Vec<_> = rows
            .iter()
            .filter(|row| row["codex_ownership"]["pending_seq"] == json!(p.seq))
            .collect();
        assert_eq!(
            terminal.len(),
            1,
            "exactly one durable terminal for {}",
            p.seq
        );
        let terminal = terminal[0];
        match &p.claim.evidence {
            binding_events::codex::ClaimEvidence::NativeHook { event, .. }
                if event == "user_prompt_submit" =>
            {
                assert!(terminal["new"]["rejected"].is_object());
                assert_eq!(
                    terminal["codex_ownership"]["claim"],
                    serde_json::to_value(&p.claim).unwrap()
                );
                assert_eq!(
                    terminal["new"]["rejected"]["reason"],
                    "superseded_by_executed_resume"
                );
                assert!(
                    terminal["seq"].as_u64().unwrap() < proof.seq,
                    "companions close before proof"
                );
            }
            _ => {
                assert_eq!(terminal["new"]["resolved"]["pending_seq"], p.seq);
                assert_eq!(terminal["seq"], proof.seq);
            }
        }
    }
    let binding = h.consumer().expect("published runtime binding");
    assert_eq!(binding.session_id.as_deref(), Some(ID));
    assert_eq!(Path::new(&binding.output_path), path);
    let marker: Value = serde_json::from_slice(&fs::read(h.marker()).unwrap()).unwrap();
    assert_eq!(marker["rollout_path"], path.to_string_lossy().as_ref());
    assert_eq!(marker["codex_ownership"]["proof_seq"], proof.seq);
}

fn pending_pair(h: &Fixture) -> Vec<binding_events::codex::ClaimRecord> {
    assert_eq!(
        hook(h, "session-start", ID, "resume", Some(&h.path(ID))),
        IngressOutcome::Durable(DurableKind::Pending)
    );
    assert_eq!(
        hook_with_source(h, "user-prompt-submit", ID, None, Some(&h.path(ID))),
        IngressOutcome::Durable(DurableKind::Pending)
    );
    let fold = h.fold();
    assert!(fold.verified.is_none());
    assert_eq!(fold.pending.len(), 2);
    fold.pending
}

#[test]
fn executed_resume_companion_invalid_terminal_candidate_appends_nothing() {
    let h = resume();
    h.header(ID, false);
    let pending = pending_pair(&h);
    let before = log_bytes(&h);
    let notice = binding_events::subscribe_binding_events(584_504).unwrap();
    let seq = *notice.borrow();
    binding_events::codex::corrupt_companion_candidate_for_tests();
    let result = record(&h, ID, &h.path(ID));
    assert!(
        !matches!(result, Ok(true)),
        "invalid candidate must fail closed: {result:?}"
    );
    assert_eq!(log_bytes(&h), before);
    assert_eq!(*notice.borrow(), seq);
    assert_eq!(h.fold().pending, pending);
    assert!(h.fold().verified.is_none());
    h.absent();
}

#[test]
fn executed_resume_companion_conflict_blocks_without_partial_settlement() {
    for case in [
        "uuid",
        "descriptor",
        "clear",
        "compact",
        "fork",
        "startup",
        "ups_clear",
        "ups_compact",
        "anchor",
        "explicit",
    ] {
        let h = resume();
        h.header(ID, false);
        pending_pair(&h);
        match case {
            "uuid" => {
                h.header(CHILD, false);
                hook_with_source(&h, "user-prompt-submit", CHILD, None, Some(&h.path(CHILD)));
            }
            "descriptor" => {
                let other = h
                    .context
                    .provider_root
                    .as_ref()
                    .unwrap()
                    .join("other")
                    .join(format!("rollout-{ID}.jsonl"));
                write_rollout(&h, &other, ID, json!("cli"));
                assert_ne!(
                    fs::metadata(&other).unwrap().ino(),
                    fs::metadata(h.path(ID)).unwrap().ino()
                );
                hook_with_source(&h, "user-prompt-submit", ID, None, Some(&other));
            }
            "ups_clear" | "ups_compact" => {
                hook(
                    &h,
                    "user-prompt-submit",
                    ID,
                    case.strip_prefix("ups_").unwrap(),
                    None,
                );
            }
            "anchor" => {
                hook(&h, "session-start", ID, "resume", None);
            }
            "explicit" => {
                let claim = binding_events::codex::Claim {
                    session_id: ID.into(),
                    path: Some(h.path(ID)),
                    evidence: binding_events::codex::ClaimEvidence::ExplicitResume {
                        executed_session_id: ID.into(),
                    },
                };
                crate::services::tmux_common::with_tmux_source_authority(
                    &h.context.tmux_session,
                    |_| {
                        binding_events::codex::commit_claim(
                            &h.context,
                            &claim,
                            binding_events::codex::Decision::Pending,
                            || Ok(()),
                        )
                        .unwrap();
                    },
                );
            }
            reason => {
                hook(&h, "session-start", ID, reason, None);
            }
        }
        let pending = h.fold().pending;
        assert_eq!(pending.len(), 3, "fixture must retain the conflict: {case}");
        let before = log_bytes(&h);
        let result = record(&h, ID, &h.path(ID));
        assert_blocked(&h, result, &before, 3);
        assert_eq!(h.fold().pending, pending, "{case}");
        assert!(
            rows(&h)
                .iter()
                .all(|row| row["new"]["rejected"].is_null() && row["new"]["resolved"].is_null()),
            "{case}"
        );
    }
}

#[test]
fn executed_resume_unregistered_old_rollout_recovers_in_one_recovery() {
    let h = resume();
    let at = (h.context.created_at - chrono::Duration::days(4)).to_rfc3339();
    let meta = json!({"type":"session_meta", "timestamp":at, "payload":{
        "id":ID,"timestamp":at,"cwd":h.root.path(),"source":"cli","originator":"codex-tui"}});
    fs::write(h.path(ID), format!("{meta}\n")).unwrap();
    assert_eq!(
        fs::read_dir(h.context.provider_root.as_ref().unwrap())
            .unwrap()
            .count(),
        1
    );
    let path = h.path(ID).canonicalize().unwrap();
    let stat = fs::metadata(&path).unwrap();
    let source = binding_events::SourceId {
        session_id: ID.into(),
        path: path.clone(),
        dev: stat.dev(),
        ino: stat.ino(),
    };
    let previous = BindingContext {
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        created_at: h.context.created_at - chrono::Duration::days(5),
        launch_mode: "fresh".into(),
        expected_native_session_id: None,
        ..h.context.clone()
    };
    let claim = binding_events::codex::Claim {
        session_id: ID.into(),
        path: Some(path),
        evidence: binding_events::codex::ClaimEvidence::NativeHook {
            event: "session_start".into(),
            source: Some("startup".into()),
            first_prompt_digest: None,
        },
    };
    crate::services::tmux_common::with_tmux_source_authority(&h.context.tmux_session, |_| {
        let fold = binding_events::codex::commit_claim(
            &previous,
            &claim,
            binding_events::codex::Decision::Verified(source),
            || Ok(()),
        )
        .unwrap();
        assert!(fold.verified.is_some());
    });
    let previous_row = rows(&h)[0].clone();
    assert!(
        h.fold().verified.is_none(),
        "the previous nonce cannot prove this execution"
    );
    let pending = pending_pair(&h);
    dedupe::reset_state_for_tests();
    binding_events::forget_channel_for_tests(584_504);
    codex_verified::clear_permissions_for_tests();
    assert_eq!(
        owner_channel_for_tmux_session(&h.context.tmux_session),
        None
    );
    h.absent();
    assert!(codex_verified::recover_discovered_codex_binding(
        &h.context.tmux_session,
        584_504
    ));
    assert_terminal_contract(&h, &pending);
    assert_eq!(rows(&h)[0], previous_row, "historical source is preserved");
    assert_eq!(rows(&h).len(), 5);
}

#[test]
fn executed_resume_companion_replay_repetition_and_late_hook_are_idempotent() {
    let h = resume();
    h.header(ID, false);
    let pending = pending_pair(&h);
    assert!(record(&h, ID, &h.path(ID)).unwrap());
    assert_terminal_contract(&h, &pending);
    let (log, fold, marker, binding) = (
        log_bytes(&h),
        h.fold(),
        fs::read(h.marker()).unwrap(),
        h.consumer().unwrap(),
    );
    dedupe::reset_state_for_tests();
    binding_events::forget_channel_for_tests(584_504);
    for _ in 0..2 {
        assert!(codex_verified::recover_discovered_codex_binding(
            &h.context.tmux_session,
            584_504
        ));
        assert_eq!(log_bytes(&h), log);
        assert_eq!(h.fold(), fold);
    }
    assert_eq!(
        hook_with_source(&h, "user-prompt-submit", ID, None, Some(&h.path(ID))),
        IngressOutcome::Durable(DurableKind::Adopted)
    );
    assert_eq!(
        hook(&h, "session-start", ID, "resume", Some(&h.path(ID))),
        IngressOutcome::Durable(DurableKind::Adopted)
    );
    assert_eq!(log_bytes(&h), log);
    assert_eq!(h.fold(), fold);
    assert_eq!(fs::read(h.marker()).unwrap(), marker);
    assert_eq!(h.consumer().unwrap(), binding);
}

#[test]
fn executed_resume_companion_partial_rejection_recovers_after_restart() {
    for phase in ["write", "sync"] {
        for multiple in [false, true] {
            let h = resume();
            h.header(ID, false);
            let mut pending = pending_pair(&h);
            if multiple {
                hook_with_source(&h, "user-prompt-submit", ID, None, None);
                pending = h.fold().pending;
                assert_eq!(pending.len(), 3);
            }
            binding_events::codex::after_companion_for_tests(move || {
                binding_events::APPEND_FAULT.with(|slot| slot.set(Some(phase)));
            });
            let result = record(&h, ID, &h.path(ID));
            binding_events::APPEND_FAULT.with(|slot| slot.set(None));
            assert!(result.is_err(), "{phase}/{multiple}: {result:?}");
            let fold = h.fold();
            assert!(fold.verified.is_none());
            assert_eq!(fold.pending.len(), pending.len() - 1);
            assert_eq!(
                rows(&h)
                    .iter()
                    .filter(|row| row["new"]["rejected"].is_object())
                    .count(),
                1
            );
            assert!(!fold.pending.iter().any(|p| p.seq == pending[1].seq));
            h.absent();
            dedupe::reset_state_for_tests();
            binding_events::forget_channel_for_tests(584_504);
            assert!(codex_verified::recover_discovered_codex_binding(
                &h.context.tmux_session,
                584_504
            ));
            assert_terminal_contract(&h, &pending);
        }
    }
}

#[test]
fn executed_resume_companion_publication_failure_retries_without_append() {
    let h = resume();
    h.header(ID, false);
    let pending = pending_pair(&h);
    fs::create_dir_all(h.marker()).unwrap();
    let result = record(&h, ID, &h.path(ID));
    assert!(result.is_err());
    let fold = h.fold();
    assert!(fold.pending.is_empty() && fold.verified.is_some());
    assert!(h.raw().is_none());
    let log = log_bytes(&h);
    fs::remove_dir(h.marker()).unwrap();
    dedupe::reset_state_for_tests();
    binding_events::forget_channel_for_tests(584_504);
    assert!(codex_verified::recover_discovered_codex_binding(
        &h.context.tmux_session,
        584_504
    ));
    assert_eq!(h.fold(), fold);
    assert_eq!(log_bytes(&h), log);
    assert_terminal_contract(&h, &pending);
}

#[test]
fn executed_resume_companion_replay_requires_exact_terminal_sequences() {
    let h = resume();
    h.header(ID, false);
    let pending = pending_pair(&h);
    assert!(record(&h, ID, &h.path(ID)).unwrap());
    let (original, live) = (rows(&h), h.fold());
    assert_eq!(rewrite(&h, &original).unwrap(), live);
    for case in [
        "missing",
        "anchor",
        "absent",
        "duplicate",
        "proof_first",
        "claim_id",
        "claim_path",
        "claim_event",
    ] {
        let mut changed = original.clone();
        match case {
            "missing" => {
                changed[2]["codex_ownership"]["pending_seq"] = Value::Null;
            }
            "anchor" => {
                changed[2]["codex_ownership"]["pending_seq"] = json!(pending[0].seq);
            }
            "absent" => {
                changed[2]["codex_ownership"]["pending_seq"] = json!(999);
            }
            "duplicate" => {
                changed.insert(3, changed[2].clone());
            }
            "proof_first" => {
                changed.swap(2, 3);
            }
            "claim_id" => {
                changed[2]["codex_ownership"]["claim"]["session_id"] = json!(CHILD);
                changed[2]["new"]["rejected"]["payload_session_id"] = json!(CHILD);
            }
            "claim_path" => {
                let other = h
                    .context
                    .provider_root
                    .as_ref()
                    .unwrap()
                    .join("other.jsonl");
                changed[2]["codex_ownership"]["claim"]["path"] = json!(other);
                changed[2]["new"]["rejected"]["payload_transcript_path"] = json!(other);
            }
            "claim_event" => {
                changed[2]["codex_ownership"]["claim"]["evidence"]["native_hook"]["event"] =
                    json!("session_start");
                changed[2]["codex_ownership"]["claim"]["evidence"]["native_hook"]["source"] =
                    json!("resume");
                changed[2]["evidence"]["hook_event"] = json!("session_start");
                changed[2]["cause"] = json!("resume");
            }
            _ => unreachable!(),
        }
        for (index, row) in changed.iter_mut().enumerate() {
            row["seq"] = json!(index + 1);
            row["codex_ownership"]["seq"] = json!(index + 1);
        }
        assert!(
            rewrite(&h, &changed).is_err(),
            "strict replay must reject {case}"
        );
    }
}

#[test]
fn executed_resume_companion_revalidates_entire_set_at_every_append() {
    for after_rejection in [false, true] {
        for case in ["nonce", "context", "inode", "header"] {
            let h = resume();
            h.header(ID, false);
            let pending = pending_pair(&h);
            let path = h.path(ID);
            let held = path.with_extension("held");
            let nonce_path = PathBuf::from(crate::services::tmux_common::session_temp_path(
                &h.context.tmux_session,
                "spawn_nonce",
            ));
            let (native_bytes, context_bytes, nonce_bytes) = (
                fs::read(&path).unwrap(),
                fs::read(&h.canonical).unwrap(),
                fs::read(&nonce_path).unwrap(),
            );
            let (changed_path, changed_held, changed_context, changed_nonce) = (
                path.clone(),
                held.clone(),
                h.canonical.clone(),
                nonce_path.clone(),
            );
            let action = move || match case {
                "nonce" => {
                    fs::write(changed_nonce, "b".repeat(32)).unwrap();
                }
                "context" => {
                    let mut context: Value =
                        serde_json::from_slice(&fs::read(&changed_context).unwrap()).unwrap();
                    context["first_prompt_digest"] = json!(format!("sha256:{}", "f".repeat(64)));
                    fs::write(changed_context, serde_json::to_vec(&context).unwrap()).unwrap();
                }
                "inode" => {
                    fs::rename(&changed_path, &changed_held).unwrap();
                    fs::copy(changed_held, changed_path).unwrap();
                }
                "header" => {
                    let mut header: Value =
                        serde_json::from_slice(&fs::read(&changed_path).unwrap()).unwrap();
                    header["payload"]["source"] =
                        json!({"subagent":{"thread_spawn":{"parent_thread_id":CHILD}}});
                    fs::write(changed_path, format!("{header}\n")).unwrap();
                }
                _ => unreachable!(),
            };
            if after_rejection {
                binding_events::codex::after_companion_for_tests(action);
            } else {
                codex_verified::before_commit_for_tests(action);
            }
            let result = record(&h, ID, &path);
            assert!(
                result.is_err(),
                "{case}/after={after_rejection}: {result:?}"
            );
            let fold = h.fold();
            assert!(fold.verified.is_none());
            assert_eq!(fold.pending.len(), 2 - usize::from(after_rejection));
            assert_eq!(
                rows(&h)
                    .iter()
                    .filter(|row| row["new"]["rejected"].is_object())
                    .count(),
                usize::from(after_rejection)
            );
            assert!(fold.pending.iter().any(|p| p.seq == pending[0].seq));
            h.absent();
            if case == "inode" {
                fs::remove_file(&path).unwrap();
                fs::rename(held, &path).unwrap();
            } else if case == "header" {
                fs::write(&path, &native_bytes).unwrap();
            }
            fs::write(&h.canonical, &context_bytes).unwrap();
            fs::write(&nonce_path, &nonce_bytes).unwrap();
            dedupe::reset_state_for_tests();
            binding_events::forget_channel_for_tests(584_504);
            assert!(codex_verified::recover_discovered_codex_binding(
                &h.context.tmux_session,
                584_504
            ));
            assert_terminal_contract(&h, &pending);
        }
    }
}

#[test]
fn executed_resume_companion_late_ups_after_partial_failure_gets_its_own_terminal() {
    let h = resume();
    h.header(ID, false);
    let mut pending = pending_pair(&h);
    binding_events::codex::after_companion_for_tests(|| {
        binding_events::APPEND_FAULT.with(|slot| slot.set(Some("write")));
    });
    let result = record(&h, ID, &h.path(ID));
    binding_events::APPEND_FAULT.with(|slot| slot.set(None));
    assert!(result.is_err());
    assert_eq!(h.fold().pending.len(), 1);
    assert_eq!(
        hook_with_source(&h, "user-prompt-submit", ID, None, Some(&h.path(ID))),
        IngressOutcome::Durable(DurableKind::Pending)
    );
    let late = h
        .fold()
        .pending
        .into_iter()
        .find(|p| p.seq != pending[0].seq)
        .expect("late UPS Pending");
    assert!(late.seq > pending[1].seq);
    pending.push(late);
    assert!(codex_verified::recover_discovered_codex_binding(
        &h.context.tmux_session,
        584_504
    ));
    assert_terminal_contract(&h, &pending);
}

#[test]
fn executed_resume_companion_late_ups_contends_until_proof_publication() {
    let h = resume();
    h.header(ID, false);
    let pending = pending_pair(&h);
    let worker = std::sync::Arc::new(std::sync::Mutex::new(None));
    let captured = worker.clone();
    let context = h.context.clone();
    let (root, path, log, marker) = (
        h.root.path().to_path_buf(),
        h.path(ID),
        log_path(&h),
        h.marker(),
    );
    let (contended, observed) = std::sync::mpsc::sync_channel(0);
    binding_events::codex::after_companion_for_tests(move || {
        let task = std::thread::spawn(move || {
            binding_events::set_test_root(Some(&root));
            crate::services::tmux_common::SOURCE_AUTHORITY_CONTENDED.with_borrow_mut(|slot| {
                *slot = Some(Box::new(move || contended.send(()).unwrap()));
            });
            let envelope = HookBindingEnvelope {
                context: CapturedContext::Captured(context.clone()),
                observed: ObservedHookProcess::default(),
            };
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
            let payload = json!({"session_id":ID,"transcript_path":path,"prompt":PROMPT});
            let outcome = observe_binding_hook(
                "codex",
                "user-prompt-submit",
                Some(ID),
                Some(ID),
                &payload,
                &headers,
            );
            let snapshot = (
                fs::read(log).unwrap(),
                binding_events::codex::read_ownership(&context).unwrap(),
                fs::read(marker).unwrap(),
            );
            codex_verified::clear_permissions_for_tests();
            binding_events::set_test_root(None);
            (outcome, snapshot)
        });
        *captured.lock().unwrap() = Some(task);
        observed
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the late hook actually contends on the held source authority");
    });
    assert!(record(&h, ID, &h.path(ID)).unwrap());
    let task = worker
        .lock()
        .unwrap()
        .take()
        .expect("late hook started after UPS append");
    let (outcome, (log, fold, marker)) = task.join().unwrap();
    assert_eq!(outcome, IngressOutcome::Durable(DurableKind::Adopted));
    assert_terminal_contract(&h, &pending);
    assert_eq!(
        rows(&h).len(),
        4,
        "the contending hook appends no new Pending or terminal"
    );
    assert_eq!(log_bytes(&h), log);
    assert_eq!(h.fold(), fold);
    assert_eq!(fs::read(h.marker()).unwrap(), marker);
}
