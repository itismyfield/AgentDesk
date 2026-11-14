use super::super::super::EVALUATIONS;
use super::*;
use crate::services::tui_o::store::ledger::input_notices::NoticeEvidence;

fn retained(c: &Case, actor: &mut Host, id: &str, prompt: &str) {
    let raw = row(id, prompt);
    let raw_record = raw[..raw.len() - 1].to_vec();
    actor
        .writer
        .record(LedgerEntry::InputNotice {
            entry: Box::new(NoticeEntry::Materialized {
                evidence: Box::new(NoticeEvidence {
                    record: crate::services::tui_o::shadow::SourceRange {
                        source: c.source.clone(),
                        start: 0,
                        end: raw.len() as u64,
                    },
                    raw_record,
                    prompt: Some(prompt.into()),
                    prompt_sha256: Some(hex::encode(Sha256::digest(prompt.as_bytes()))),
                    unit_key: None,
                    payload: None,
                    origin: None,
                }),
            }),
        })
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn retained_machine_policy_settles_without_origin_and_replays() {
    let c = Case::new("settle-policy");
    let mut actor = c.open();
    let prompt = "<command-name>/clear</command-name>";
    assert!(crate::services::discord::input_notice_content(prompt).is_none());
    retained(&c, &mut actor, "policy", prompt);
    actor.prepare_notices(&[]).await;
    assert_eq!(actor.notice_debt_extra(), 0);
    assert_eq!(c.notices(), 0);
    let ledger = std::fs::read_to_string(notice_ledger(c._runtime.path())).unwrap();
    assert!(ledger.contains("\"kind\":\"settled\""));
    drop(actor);
    let mut actor = c.open();
    actor.prepare_notices(&[]).await;
    assert_eq!(actor.notice_debt_extra(), 0);
}

#[tokio::test(start_paused = true)]
async fn pending_mismatch_reuses_work_on_unchanged_prepare() {
    let c = Case::new("settle-work");
    let frame = c.frame();
    let registry = history::v2::Registry::runtime().unwrap();
    let key = registration(&c.path, &c.logical, "foreign", 8911, false);
    registry
        .prepare(NAMESPACE, &registered_input(&key, &frame))
        .unwrap();
    c.input("mismatch", &frame);
    let mut actor = c.open();
    let fresh = actor.read_sources();
    actor.prepare_notices(&fresh).await;
    assert_eq!(actor.notice_debt_extra(), 1);
    EVALUATIONS.with(|c| c.set((0, 0)));
    for _ in 0..8 {
        actor.prepare_notices(&[]).await;
    }
    eprintln!(
        "NOTICE_PENDING_COST kind=mismatch prepares=8 evaluated_records={}",
        EVALUATIONS.with(|c| c.get().0)
    );
    assert_eq!(
        EVALUATIONS.with(|c| c.get().0),
        0,
        "unchanged pending records must not be cloned or admitted again"
    );
}

#[tokio::test(start_paused = true)]
async fn pending_reason_rotation_and_eligible_roundtrip_log_once() {
    let c = Case::new("settle-logs");
    let frame = c.frame();
    c.register(&frame);
    c.input("owned", &frame);
    c.input("direct", &frame);
    let mut actor = c.open();
    let fresh = actor.read_sources();
    let e = actor
        .writer
        .store()
        .ledger()
        .input_notices()
        .evidence
        .values()
        .find(|e| e.unit_key.as_ref() == Some(&input_key("direct")))
        .unwrap();
    let id = (e.record.source.clone(), e.record.start, e.record.end);
    EVALUATIONS.with(|c| c.set((0, 0)));
    for reason in [
        "authority_busy",
        "native_census_unavailable",
        "history_snapshot_changed",
        "registration_source_mismatch",
    ] {
        actor.defer_notice(id.clone(), reason);
    }
    actor.prepare_notices(&fresh).await;
    actor.defer_notice(id.clone(), "authority_busy");
    assert_eq!(EVALUATIONS.with(|c| c.get().1), 1);
    assert!(actor.writer.notice_pending.contains_key(&id));
    drop(actor);
    let mut actor = c.open();
    EVALUATIONS.with(|c| c.set((0, 0)));
    actor.defer_notice(id, "native_census_unavailable");
    assert_eq!(
        EVALUATIONS.with(|c| c.get().1),
        1,
        "restart may log once more"
    );
}

#[tokio::test(start_paused = true)]
async fn retained_direct_full_frame_twin_survives_history_failure() {
    let c = Case::new("settle-twin-history");
    let frame = c.frame();
    c.register(&frame);
    c.input("owned", &frame);
    let mut actor = c.open();
    c.run(&mut actor).await;
    c.input("direct", &frame);
    let fresh = actor.read_sources();
    let path = c._guard.path().join("runtime/injected_frames.json");
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"unreadable history").unwrap();
    actor.prepare_notices(&fresh).await;
    assert!(
        actor
            .writer
            .store()
            .ledger()
            .excluded(&input_key("direct"))
            .is_none()
    );
    assert_eq!(actor.notice_debt_extra(), 1);
    assert_eq!(c.notices(), 0);
    std::fs::write(&path, original).unwrap();
    actor.prepare_notices(&[]).await;
    actor
        .deliver_owed(&crate::services::tui_o::writer::progress::Publisher::new(
            CHANNEL,
        ))
        .await;
    assert_eq!(c.notices(), 1);
    assert_eq!(actor.notice_debt_extra(), 0);
}

#[tokio::test(start_paused = true)]
async fn pending_scope_rechecks_once_for_each_dependency_change() {
    let c = Case::new("settle-generations");
    let frame = c.frame();
    let nonce = "73320000000000000000000000000811";
    let registry = history::v2::Registry::runtime().unwrap();
    let key = registration(&c.path, &c.logical, nonce, 8912, false);
    registry
        .prepare(NAMESPACE, &registered_input(&key, &frame))
        .unwrap();
    c.input("mismatch", &frame);
    let mut actor = c.open();
    let fresh = actor.read_sources();
    actor.prepare_notices(&fresh).await;
    let history_path = c._guard.path().join("runtime/injected_frames.json");
    let binding_path = crate::services::tui_prompt_dedupe::binding_events::log_path(CHANNEL)
        .unwrap()
        .unwrap();
    let context_path =
        crate::services::tui_prompt_dedupe::binding_context::context_path("claude", nonce).unwrap();
    for dependency in ["history", "native", "context", "binding"] {
        EVALUATIONS.with(|c| c.set((0, 0)));
        history::notice_owner::SNAPSHOT_READS.with(|c| c.set(0));
        for _ in 0..4 {
            actor.prepare_notices(&[]).await;
        }
        assert_eq!(EVALUATIONS.with(|c| c.get().0), 0, "before {dependency}");
        assert_eq!(history::notice_owner::SNAPSHOT_READS.with(|c| c.get()), 0);
        match dependency {
            "history" => {
                std::fs::write(&history_path, std::fs::read(&history_path).unwrap()).unwrap()
            }
            "native" => append(&c.path, b" "),
            "context" => {
                std::fs::create_dir_all(context_path.parent().unwrap()).unwrap();
                std::fs::write(&context_path, b"{}").unwrap();
            }
            "binding" => {
                std::fs::write(&binding_path, std::fs::read(&binding_path).unwrap()).unwrap()
            }
            _ => unreachable!(),
        }
        actor.prepare_notices(&[]).await;
        assert_eq!(
            EVALUATIONS.with(|c| c.get().0),
            1,
            "one record reevaluation after {dependency}"
        );
        assert_eq!(
            history::notice_owner::SNAPSHOT_READS.with(|c| c.get()),
            1,
            "one scope snapshot after {dependency}"
        );
        actor.prepare_notices(&[]).await;
        assert_eq!(
            EVALUATIONS.with(|c| c.get().0),
            1,
            "no repeated work after {dependency}"
        );
    }
    assert_eq!(actor.notice_debt_extra(), 1);
}

#[tokio::test(start_paused = true)]
async fn unavailable_scope_reuses_work_and_retries_after_five_seconds() {
    let c = Case::new("settle-unavailable");
    let frame = c.frame();
    c.register(&frame);
    c.input("missing", &frame);
    let mut actor = c.open();
    let fresh = actor.read_sources();
    let parked = c.path.with_extension("parked");
    std::fs::rename(&c.path, &parked).unwrap();
    actor.prepare_notices(&fresh).await;
    EVALUATIONS.with(|c| c.set((0, 0)));
    history::notice_owner::SNAPSHOT_READS.with(|c| c.set(0));
    for _ in 0..8 {
        actor.prepare_notices(&[]).await;
    }
    assert_eq!(EVALUATIONS.with(|c| c.get().0), 0);
    assert_eq!(history::notice_owner::SNAPSHOT_READS.with(|c| c.get()), 0);
    tokio::time::advance(std::time::Duration::from_secs(5)).await;
    actor.prepare_notices(&[]).await;
    assert_eq!(EVALUATIONS.with(|c| c.get().0), 1);
    actor.prepare_notices(&[]).await;
    assert_eq!(EVALUATIONS.with(|c| c.get().0), 1);
    assert_eq!(actor.notice_debt_extra(), 1);
    std::fs::rename(parked, &c.path).unwrap();
    actor.prepare_notices(&[]).await;
    c.excluded(&mut actor, "missing");
}

#[tokio::test(start_paused = true)]
async fn materialized_enrichment_reopens_only_its_changed_generation() {
    let c = Case::new("settle-evidence-generation");
    let frame = c.frame();
    c.register(&frame);
    let mut actor = host(&c.o, c.port.clone(), Alarms::default(), u64::MAX);
    start_migration(&mut actor);
    c.input("enrich", &frame);
    retained(&c, &mut actor, "enrich", &frame);
    actor.prepare_notices(&[]).await;
    EVALUATIONS.with(|c| c.set((0, 0)));
    actor.prepare_notices(&[]).await;
    assert_eq!(EVALUATIONS.with(|c| c.get().0), 0);
    let mut evidence = actor
        .writer
        .store()
        .ledger()
        .input_notices()
        .evidence
        .values()
        .next()
        .unwrap()
        .clone();
    evidence.unit_key = Some(input_key("enrich"));
    evidence.payload = crate::services::discord::input_notice_content(&frame);
    actor
        .writer
        .record(LedgerEntry::InputNotice {
            entry: Box::new(NoticeEntry::Materialized {
                evidence: Box::new(evidence),
            }),
        })
        .unwrap();
    actor.prepare_notices(&[]).await;
    assert_eq!(EVALUATIONS.with(|c| c.get().0), 1);
    actor.prepare_notices(&[]).await;
    assert_eq!(EVALUATIONS.with(|c| c.get().0), 1);
    assert_eq!(actor.notice_debt_extra(), 1);
}

#[tokio::test(start_paused = true)]
async fn old_source_owned_record_expires_but_direct_obligation_posts() {
    let c = Case::new("settle-source-boundary");
    let frame = c.frame();
    c.register(&frame);
    c.input("owned", &frame);
    c.input("direct", "still owes a direct notice");
    let mut actor = c.open();
    let fresh = actor.read_sources();
    assert_eq!(
        actor.writer.store().ledger().input_notices().evidence.len(),
        2
    );
    let next_path = c.path.with_extension("successor");
    std::fs::write(&next_path, b"").unwrap();
    let next =
        crate::services::tui_o::shadow::binding_reader::source_id_for("successor", &next_path)
            .unwrap();
    let proposal = crate::services::tui_prompt_dedupe::binding_events::Proposal {
        channel_id: CHANNEL,
        provider: "claude",
        tmux_session: &c.logical,
        session_id: Some("successor"),
        path: next_path.to_str().unwrap(),
        replaced: Some((c.path.to_str().unwrap(), Some("native"))),
        cause: crate::services::tui_prompt_dedupe::binding_events::CauseSource::Hook(
            crate::services::tui_prompt_dedupe::binding_events::BindingCause::Resume,
        ),
        hook: None,
    };
    assert_eq!(
        crate::services::tui_prompt_dedupe::binding_events::record_verified(&proposal, &next)
            .unwrap(),
        crate::services::tui_prompt_dedupe::binding_events::Committed::Appended
    );
    actor.prepare_notices(&fresh).await;
    let history = actor.bindings.binding_history(CHANNEL);
    let state = actor.writer.store().ledger().input_notices();
    assert!(
        state.settled.values().any(|s| s.expired_after.is_some()),
        "history={history:?} settled={:?} evidence={:?}",
        state.settled,
        state
            .evidence
            .values()
            .map(|e| (&e.record, &e.origin))
            .collect::<Vec<_>>()
    );
    assert!(
        actor
            .writer
            .store()
            .ledger()
            .excluded(&input_key("direct"))
            .is_none()
    );
    actor
        .deliver_owed(&crate::services::tui_o::writer::progress::Publisher::new(
            CHANNEL,
        ))
        .await;
    assert_eq!(c.notices(), 1);
    assert_eq!(actor.notice_debt_extra(), 0);
    let before = actor
        .writer
        .store()
        .ledger()
        .input_notices()
        .settled
        .clone();
    drop(actor);
    let mut actor = c.open();
    actor.prepare_notices(&[]).await;
    assert_eq!(
        actor.writer.store().ledger().input_notices().settled,
        before
    );
    assert_eq!(c.notices(), 1);
}

#[tokio::test(start_paused = true)]
async fn retained_356_record_fixture_settles_311_and_stops_repeated_heavy_work() {
    let c = Case::new("settle-356");
    let mut actor = c.open();
    for i in 0..311 {
        let id = format!("owned-{i}");
        let nonce = format!("fixture-attempt-{i}");
        let frame = format!("[User: ann (ID: 7)]\nretained input {i}");
        let start = std::fs::metadata(&c.path).unwrap().len();
        history::register(("claude", &c.logical, &nonce, "7"), &c.path, start, &frame).unwrap();
        c.input(&id, &frame);
        assert_eq!(
            history::receipt(&nonce, &c.path, start, "user", Some(&id), &frame),
            Some(true)
        );
        let raw = row(&id, &frame);
        let record = crate::services::tui_o::shadow::CapturedRecord {
            start,
            end: start + raw.len() as u64,
            line: raw[..raw.len() - 1].to_vec(),
        };
        let origin = actor.sources.notice_origin(&c.source, &record).unwrap();
        actor
            .writer
            .record(LedgerEntry::InputNotice {
                entry: Box::new(NoticeEntry::Materialized {
                    evidence: Box::new(NoticeEvidence {
                        record: origin.record.clone(),
                        raw_record: record.line,
                        prompt: Some(frame.clone()),
                        prompt_sha256: Some(hex::encode(Sha256::digest(frame.as_bytes()))),
                        unit_key: Some(input_key(&id)),
                        payload: crate::services::discord::input_notice_content(&frame),
                        origin: Some(Box::new(origin)),
                    }),
                }),
            })
            .unwrap();
    }
    for i in 0..45 {
        c.input(&format!("direct-{i}"), &format!("direct input {i}"));
    }
    let fresh = actor.read_sources();
    assert_eq!(
        actor.writer.store().ledger().input_notices().evidence.len(),
        356
    );
    actor.prepare_notices(&fresh).await;
    let ledger = actor.writer.store().ledger();
    let state = ledger.input_notices();
    assert_eq!(state.settled.len(), 311);
    assert_eq!(
        state
            .evidence
            .iter()
            .filter(|(id, e)| !state.terminal(ledger, id, e))
            .count(),
        45
    );
    EVALUATIONS.with(|c| c.set((0, 0)));
    history::notice_owner::SNAPSHOT_READS.with(|c| c.set(0));
    for _ in 0..10 {
        actor.prepare_notices(&[]).await;
    }
    assert_eq!(EVALUATIONS.with(|c| c.get()), (0, 0));
    assert_eq!(history::notice_owner::SNAPSHOT_READS.with(|c| c.get()), 0);
    actor
        .deliver_owed(&crate::services::tui_o::writer::progress::Publisher::new(
            CHANNEL,
        ))
        .await;
    assert_eq!(c.notices(), 45);
    assert_eq!(actor.notice_debt_extra(), 0);
    eprintln!(
        "NOTICE_SETTLE_FIXTURE records=356 settled={} open_after_settle=45 repeated_prepares=10 evaluated_records={} snapshot_reads={} pending_logs={} posts={}",
        actor.writer.store().ledger().input_notices().settled.len(),
        EVALUATIONS.with(|c| c.get().0),
        history::notice_owner::SNAPSHOT_READS.with(|c| c.get()),
        EVALUATIONS.with(|c| c.get().1),
        c.notices()
    );
}

#[tokio::test(start_paused = true)]
async fn retained_fresh_execution_of_same_native_source_uses_its_immutable_proof() {
    use crate::services::tui_prompt_dedupe::binding_context::{
        BindingContext, PreparedIncarnation,
    };
    let c = Case::new("settle-fresh-proof");
    let mut actor = c.open();
    let prepared = PreparedIncarnation::create(BindingContext {
        schema: 1,
        provider: "claude".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: "73320000000000000000000000000812".into(),
        tmux_session: c.logical.clone(),
        channel_id: Some(CHANNEL),
        owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
        host: None,
        expected_native_session_id: Some("native".into()),
        launch_mode: "fresh".into(),
        provider_root: None,
        first_prompt_digest: None,
        source_policy: None,
        resume_start: None,
    })
    .unwrap();
    let frame = c.frame();
    let mut key = registration(
        &c.path,
        &c.logical,
        &prepared.context.execution_nonce,
        8913,
        false,
    );
    key["attempt"]["target"]["root"] = prepared.context.owner_runtime_root.clone().into();
    let registry = history::v2::Registry::runtime().unwrap();
    registry
        .prepare(NAMESPACE, &registered_input(&key, &frame))
        .unwrap();
    c.input("fresh-owned", &frame);
    let fresh = actor.read_sources();
    actor.prepare_notices(&fresh).await;
    c.excluded(&mut actor, "fresh-owned");
    assert_eq!(
        actor.writer.store().ledger().input_notices().settled.len(),
        1
    );
    c.input("fresh-direct-twin", &frame);
    c.run(&mut actor).await;
    assert_eq!(c.notices(), 1);
    assert!(
        actor
            .writer
            .store()
            .ledger()
            .excluded(&input_key("fresh-direct-twin"))
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn unrelated_missing_context_blocks_only_its_frame_and_recovery_rechecks() {
    use crate::services::tui_prompt_dedupe::binding_context::{
        BindingContext, PreparedIncarnation,
    };
    for planned in [false, true] {
        let c = Case::new(if planned {
            "context-planned"
        } else {
            "context-bound"
        });
        let a = c.frame();
        let b = format!("[User: imessage (ID: 1)]\nother frame {}", c.logical);
        c.v2(&a, 8920, false);
        let prepared = PreparedIncarnation::create(BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: "73320000000000000000000000000813".into(),
            tmux_session: c.logical.clone(),
            channel_id: Some(CHANNEL),
            owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: Some("native".into()),
            launch_mode: "resume".into(),
            provider_root: None,
            first_prompt_digest: None,
            source_policy: None,
            resume_start: None,
        })
        .unwrap();
        let mut key = registration(
            &c.path,
            &c.logical,
            &prepared.context.execution_nonce,
            8921,
            planned,
        );
        key["attempt"]["target"]["root"] = prepared.context.owner_runtime_root.clone().into();
        let registry = history::v2::Registry::runtime().unwrap();
        registry
            .prepare(NAMESPACE, &registered_input(&key, &b))
            .unwrap();
        let bytes = std::fs::read(&prepared.path).unwrap();
        std::fs::remove_file(&prepared.path).unwrap();
        c.input("context-a-owned", &a);
        c.input("context-a-direct", &a);
        c.input("context-b-uncertain", &b);
        let mut actor = c.open();
        c.run(&mut actor).await;
        assert_eq!(
            c.notices(),
            1,
            "unrelated missing context must not suppress A's direct twin"
        );
        assert!(
            actor
                .writer
                .store()
                .ledger()
                .latest_piece(&input_key("context-a-direct"), 0)
                .is_some()
        );
        assert!(
            actor
                .writer
                .store()
                .ledger()
                .latest_piece(&input_key("context-b-uncertain"), 0)
                .is_none()
        );
        assert!(
            actor
                .writer
                .store()
                .ledger()
                .excluded(&input_key("context-b-uncertain"))
                .is_none()
        );
        assert_eq!(
            actor.notice_debt_extra(),
            1,
            "same-hash B remains pending, including planned registrations"
        );
        EVALUATIONS.with(|c| c.set((0, 0)));
        actor.prepare_notices(&[]).await;
        assert_eq!(EVALUATIONS.with(|c| c.get().0), 0);
        std::fs::write(&prepared.path, bytes).unwrap();
        actor.prepare_notices(&[]).await;
        assert!(
            EVALUATIONS.with(|c| c.get().0) > 0,
            "restoring the watched context must re-evaluate B"
        );
        if planned {
            assert_eq!(
                actor.notice_debt_extra(),
                1,
                "a recovered planned registration still needs its source"
            );
            registry
                .bind(
                    NAMESPACE,
                    &key,
                    &history::v2::Source {
                        target: key["attempt"]["target"].clone(),
                        provider: "claude".into(),
                        session_id: "native".into(),
                        transcript: c.path.clone(),
                        file: [c.source.dev, c.source.ino],
                        offset: 0,
                        binding_watermark: None,
                    },
                )
                .unwrap();
            actor.prepare_notices(&[]).await;
        }
        assert_eq!(
            actor
                .writer
                .store()
                .ledger()
                .excluded(&input_key("context-b-uncertain")),
            Some("input_notice_source")
        );
        assert_eq!(actor.notice_debt_extra(), 0);
        assert_eq!(c.notices(), 1);
    }
}
