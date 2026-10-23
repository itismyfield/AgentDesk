use super::*;
use p5::codex::{Claim, ClaimEvidence, Decision};
use serde_json::{Value, json};

fn log_path(pane: &Pane) -> PathBuf {
    p5::test_root()
        .unwrap()
        .join(p5::BINDING_EVENTS_DIR)
        .join(format!("{}.log", pane.channel))
}

fn rows(pane: &Pane) -> Vec<Value> {
    std::fs::read_to_string(log_path(pane))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn rewrite_rows(pane: &Pane, mut rows: Vec<Value>) {
    for (index, row) in rows.iter_mut().enumerate() {
        row["seq"] = json!(index + 1);
        if let Some(ownership) = row.get_mut("codex_ownership") {
            ownership["seq"] = json!(index + 1);
        }
    }
    let bytes: String = rows.iter().map(|row| format!("{row}\n")).collect();
    std::fs::write(log_path(pane), bytes).unwrap();
    p5::forget_channel_for_tests(pane.channel);
}

fn source(pane: &Pane) -> p5::SourceId {
    p5::records_strict(pane.channel)
        .unwrap()
        .unwrap()
        .into_iter()
        .rev()
        .find_map(|event| match event.new {
            p5::BindingTarget::Source(source) | p5::BindingTarget::Resolved { source, .. } => {
                Some(source)
            }
            _ => None,
        })
        .unwrap()
}

fn install_context(pane: &mut Pane, context: BindingContext) {
    pane.context_path = PreparedIncarnation::create(context.clone()).unwrap().path;
    std::fs::write(
        tc::session_temp_path(&pane.name, "spawn_nonce"),
        &context.execution_nonce,
    )
    .unwrap();
    pane.context = context;
}

fn resume_with_companions(pane: &mut Pane) -> (u64, u64) {
    let source = source(pane);
    let context = BindingContext {
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        launch_mode: "resume".into(),
        expected_native_session_id: Some(source.session_id.clone()),
        source_policy: Some("verified".into()),
        provider_root: Some(
            pane.context
                .provider_root
                .as_ref()
                .unwrap()
                .canonicalize()
                .unwrap(),
        ),
        ..pane.context.clone()
    };
    install_context(pane, context);
    let native = |event: &str, reason: Option<&str>| Claim {
        session_id: source.session_id.clone(),
        path: Some(source.path.clone()),
        evidence: ClaimEvidence::NativeHook {
            event: event.into(),
            source: reason.map(str::to_owned),
            first_prompt_digest: None,
        },
    };
    let (anchor, companion) = tc::with_tmux_source_authority(&pane.name, |_| {
        p5::codex::commit_claim_published(
            &pane.context,
            &native("session_start", Some("resume")),
            Decision::Pending,
            None,
            || Ok(()),
        )
        .unwrap();
        let pending = p5::codex::commit_claim_published(
            &pane.context,
            &native("user_prompt_submit", None),
            Decision::Pending,
            None,
            || Ok(()),
        )
        .unwrap();
        assert_eq!(
            pending.pending.len(),
            2,
            "producer creates both Pending claims"
        );
        let (anchor, companion) = (pending.pending[0].seq, pending.pending[1].seq);
        let executed = Claim {
            session_id: source.session_id.clone(),
            path: Some(source.path.clone()),
            evidence: ClaimEvidence::ExplicitResume {
                executed_session_id: source.session_id.clone(),
            },
        };
        let settled = p5::codex::commit_executed_resume_published(
            &pane.context,
            &executed,
            source.clone(),
            || Ok(()),
            |_| Ok(()),
        )
        .unwrap();
        assert!(settled.pending.is_empty() && !settled.conflicted);
        assert_eq!(
            settled.verified.unwrap().ownership.pending_seq,
            Some(anchor)
        );
        (anchor, companion)
    });
    let rows = rows(pane);
    let terminal = &rows[rows.len() - 2];
    assert_eq!(terminal["codex_ownership"]["pending_seq"], companion);
    assert_eq!(
        terminal["new"]["rejected"]["reason"],
        "superseded_by_executed_resume"
    );
    assert_eq!(
        rows.last().unwrap()["new"]["resolved"]["pending_seq"],
        anchor
    );
    assert!(
        dedupe::codex_verified::record_executed_resume(
            &pane.context,
            &source.session_id,
            &source.path,
            0,
        )
        .unwrap(),
        "the real resume publisher installs the proof marker and runtime binding"
    );
    (anchor, companion)
}

fn next_shadow_source(pane: &mut Pane) {
    let id = uuid::Uuid::new_v4();
    let context = BindingContext {
        created_at: chrono::Utc::now(),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        launch_mode: "fresh".into(),
        expected_native_session_id: None,
        source_policy: Some("shadow".into()),
        ..pane.context.clone()
    };
    install_context(pane, context);
    pane.source = pane
        .context
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{id}.jsonl"));
    let header = json!({"type":"session_meta","payload":{
        "id":id.to_string(),"source":"cli","cwd":"/private/tmp",
        "timestamp":(pane.context.created_at + chrono::Duration::seconds(1)).to_rfc3339()
    }});
    std::fs::write(&pane.source, format!("{header}\n")).unwrap();
    dedupe::register_launched_tmux_runtime_binding(
        &pane.name,
        binding(&pane.source, &id.to_string()),
    );
    let current = rows(pane).pop().unwrap();
    assert_eq!(current["new"]["source"]["session_id"], id.to_string());
    assert_eq!(current["execution_nonce"], pane.context.execution_nonce);
}

fn assert_exact_end(pane: &Pane, case: &str) {
    let expected_source = source(pane);
    let expected_seq = rows(pane).last().unwrap()["seq"].clone();
    pane.end(true);
    let records = pane.records();
    assert_eq!(
        records.len(),
        1,
        "{case}: settled history must record exactly one End"
    );
    assert_eq!(
        records[0]["source"],
        serde_json::to_value(expected_source).unwrap(),
        "{case}: exact source"
    );
    assert_eq!(
        records[0]["execution_nonce"], pane.context.execution_nonce,
        "{case}: exact execution nonce"
    );
    assert_eq!(records[0]["tmux_session"], pane.name, "{case}: exact pane");
    assert_eq!(
        records[0]["binding_seq"], expected_seq,
        "{case}: exact captured log revision"
    );
    assert_eq!(
        attempt::ticket(pane.channel, &pane.name).as_deref(),
        records[0]["end_id"].as_str(),
        "{case}: durable End installs its exact ticket"
    );
}

#[test]
fn two_historical_resume_companion_pairs_allow_exact_clear_end_and_ticket() {
    let fixture = Fixture::new();
    let mut pane = Pane::start(fixture.logs.path());
    let first = resume_with_companions(&mut pane);
    let second = resume_with_companions(&mut pane);
    assert!(
        first.1 < second.0,
        "both completed incarnations remain in the same pane log"
    );
    next_shadow_source(&mut pane);
    assert_eq!(rows(&pane).len(), 10);
    assert_exact_end(&pane, "two settled historical resume pairs");
}

#[test]
fn current_resume_companion_pair_allows_exact_clear_end_and_ticket() {
    let fixture = Fixture::new();
    let mut pane = Pane::start(fixture.logs.path());
    resume_with_companions(&mut pane);
    assert_eq!(rows(&pane).len(), 5);
    let mut rows = rows(&pane);
    let mut neutral = rows[4].clone();
    neutral.as_object_mut().unwrap().remove("codex_ownership");
    let current = neutral["new"]["resolved"]["source"].clone();
    neutral["old"] = current.clone();
    neutral["new"] = json!({"source":current});
    neutral["cause"] = json!("startup");
    neutral["evidence"]["hook_event"] = Value::Null;
    rows.push(neutral);
    let mut audit = rows[3].clone();
    audit.as_object_mut().unwrap().remove("codex_ownership");
    audit["new"]["rejected"]["reason"] = json!("audit only");
    rows.push(audit);
    rewrite_rows(&pane, rows);
    assert_exact_end(&pane, "current settled resume pair");
}

#[test]
fn unsettled_or_malformed_companion_history_never_records_end_or_ticket() {
    let fixture = Fixture::new();
    for case in [
        "missing_rejected",
        "missing_resolved",
        "wrong_rejected_seq",
        "wrong_resolved_seq",
        "wrong_terminal_nonce",
        "wrong_terminal_pane",
        "wrong_terminal_provider",
        "terminal_before_pending",
        "legacy_no_envelopes",
        "legacy_missing_pending_seq",
        "current_unresolved",
        "older_unresolved_before_new_source",
        "conflicting_pending",
        "conflicting_owned_proof",
        "legacy_conflict_then_old_source",
    ] {
        let mut pane = Pane::start(fixture.logs.path());
        let (anchor, companion) = resume_with_companions(&mut pane);
        next_shadow_source(&mut pane);
        let mut rows = rows(&pane);
        assert_eq!(rows.len(), 6);
        match case {
            "missing_rejected" => {
                rows.remove(3);
            }
            "missing_resolved" => {
                rows.remove(4);
            }
            "wrong_rejected_seq" => rows[3]["codex_ownership"]["pending_seq"] = json!(anchor),
            "wrong_resolved_seq" => {
                rows[4]["new"]["resolved"]["pending_seq"] = json!(companion);
                rows[4]["codex_ownership"]["pending_seq"] = json!(companion);
            }
            "wrong_terminal_nonce" => {
                rows[3]["execution_nonce"] = json!("e".repeat(32));
                rows[3]["codex_ownership"]["context"]["execution_nonce"] = json!("e".repeat(32));
            }
            "wrong_terminal_pane" => {
                rows[3]["tmux_session"] = json!(format!("{}-other", pane.name));
                rows[3]["codex_ownership"]["context"]["tmux_session"] =
                    rows[3]["tmux_session"].clone();
            }
            "wrong_terminal_provider" => rows[3]["provider"] = json!("claude"),
            "terminal_before_pending" => {
                let terminal = rows.remove(3);
                rows.insert(1, terminal);
                rows[1]["codex_ownership"]["pending_seq"] = json!(4);
                rows[4]["new"]["resolved"]["pending_seq"] = json!(3);
                rows[4]["codex_ownership"]["pending_seq"] = json!(3);
            }
            "legacy_no_envelopes" => {
                for row in &mut rows[1..5] {
                    row.as_object_mut().unwrap().remove("codex_ownership");
                }
            }
            "legacy_missing_pending_seq" => {
                rows[3]["codex_ownership"]
                    .as_object_mut()
                    .unwrap()
                    .remove("pending_seq");
            }
            "older_unresolved_before_new_source" => {
                rows.drain(3..5);
            }
            "conflicting_owned_proof" => {
                let mut conflict = rows[4].clone();
                let current = conflict["new"]["resolved"]["source"].clone();
                conflict["old"] = current.clone();
                conflict["new"] = json!({"source":current});
                conflict["codex_ownership"]["pending_seq"] = Value::Null;
                conflict["codex_ownership"]["claim"]["evidence"] =
                    rows[1]["codex_ownership"]["claim"]["evidence"].clone();
                conflict["evidence"]["hook_event"] = json!("session_start");
                rows.insert(5, conflict);
            }
            "legacy_conflict_then_old_source" => {
                let mut conflict = rows[5].clone();
                conflict["execution_nonce"] = rows[4]["execution_nonce"].clone();
                let mut restored = conflict.clone();
                restored["old"] = conflict["new"]["source"].clone();
                restored["new"] = json!({"source":rows[4]["new"]["resolved"]["source"]});
                rows.insert(5, conflict);
                rows.insert(6, restored);
            }
            "current_unresolved" | "conflicting_pending" => {
                let mut pending = rows[1].clone();
                pending.as_object_mut().unwrap().remove("codex_ownership");
                pending["execution_nonce"] = json!(pane.context.execution_nonce);
                rows.push(pending.clone());
                if case == "conflicting_pending" {
                    pending["new"]["pending"]["payload_session_id"] =
                        json!(uuid::Uuid::new_v4().to_string());
                    rows.push(pending);
                }
            }
            _ => unreachable!(),
        }
        rewrite_rows(&pane, rows);
        pane.end(true);
        pane.assert_no_end(case);
    }
}
