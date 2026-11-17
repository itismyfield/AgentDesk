use super::*;
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};
use crate::services::tui_prompt_dedupe::binding_events as p5;

fn launch(c: &Case, resume: bool, native: &str) -> PreparedIncarnation {
    let prepared = if resume {
        PreparedIncarnation::prepare_at(
            "claude",
            &c.logical,
            Some(CHANNEL),
            Some(native),
            true,
            None,
        )
        .unwrap()
    } else {
        PreparedIncarnation::create(BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: OWNER_NONCE.into(),
            tmux_session: c.logical.clone(),
            channel_id: Some(CHANNEL),
            owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: Some(native.into()),
            launch_mode: "fresh".into(),
            provider_root: None,
            first_prompt_digest: None,
            source_policy: None,
            resume_start: None,
        })
        .unwrap()
    };
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&c.logical, "spawn_nonce"),
        &prepared.context.execution_nonce,
    )
    .unwrap();
    let proposal = p5::Proposal {
        channel_id: CHANNEL,
        provider: "claude",
        tmux_session: &c.logical,
        session_id: Some(&c.source.session_id),
        path: c.path.to_str().unwrap(),
        replaced: None,
        cause: p5::CauseSource::Launch,
        hook: None,
    };
    assert_eq!(
        p5::record_verified(&proposal, &c.source).unwrap(),
        p5::Committed::Unchanged,
        "a tmux launch/resume of the same source keeps its attachment binding"
    );
    assert_eq!(p5::records_strict(CHANNEL).unwrap().unwrap().len(), 1);
    prepared
}

fn register_current(c: &Case, frame: &str) -> Value {
    let target =
        crate::services::claude_tui::owner_input::created_tmux_target("claude", &c.logical)
            .unwrap();
    let mut key = registration(&c.path, &c.logical, OWNER_NONCE, 8821, false);
    key["attempt"]["nonce"] = "notice-attempt".into();
    key["attempt"]["target"] = target;
    key["attempt"]["source"]["offset"] = std::fs::metadata(&c.path).unwrap().len().into();
    history::v2::Registry::runtime()
        .unwrap()
        .prepare(NAMESPACE, &registered_input(&key, frame))
        .unwrap();
    key
}

fn enqueue_collector(c: &Case, key: &Value, frame: &str) {
    let mut bytes = serde_json::to_vec(&json!({"type":"queue-operation","operation":"enqueue",
        "uuid":"resume-enqueue-only","content":frame}))
    .unwrap();
    bytes.push(b'\n');
    append(&c.path, &bytes);
    let (path, key, frame) = (c.path.clone(), key.clone(), frame.to_owned());
    let reader = std::thread::spawn(move || {
        let registry = history::v2::Registry::runtime().unwrap();
        history::v2::with_attempt(&registry, NAMESPACE, &key, || {
            crate::services::claude_tui::busy_inject::await_receipt(
                (&path, 0, "notice-attempt", &frame),
                std::time::Instant::now() + std::time::Duration::from_secs(2),
                std::time::Duration::from_millis(2),
            )
        })
        .unwrap()
    });
    assert!(
        reader.join().unwrap().is_ok(),
        "the scoped collector ended at enqueue before the user row"
    );
}

#[tokio::test(start_paused = true)]
async fn notice_owner_same_source_resume_excludes_late_imessage_row_after_reopen() {
    let c = Case::new("same-source-resume");
    let first = launch(&c, false, "native");
    let mut actor = c.open();
    c.run(&mut actor).await;
    assert!(actor.writer.store().cursor(&c.source).is_some());
    let resumed = launch(&c, true, "native");
    assert_ne!(
        first.context.execution_nonce,
        resumed.context.execution_nonce
    );
    let frame = c.frame();
    let key = register_current(&c, &frame);
    assert_eq!(
        key["attempt"]["target"]["execution_nonce"],
        resumed.context.execution_nonce
    );
    enqueue_collector(&c, &key, &frame);
    c.input("resume-owned", &frame);
    assert!(
        matches!(
            history::observed_attempt_checked(
                "claude",
                &c.logical,
                std::slice::from_ref(&frame),
                Some("resume-owned")
            )
            .unwrap(),
            history::ObservedAttempt::Pending
        ),
        "no exact UUID receipt bypasses census attribution"
    );
    c.run(&mut actor).await;
    let evidence = actor
        .writer
        .store()
        .ledger()
        .input_notices()
        .evidence
        .values()
        .find(|e| e.unit_key.as_ref() == Some(&input_key("resume-owned")))
        .unwrap();
    assert_eq!(
        evidence.origin.as_ref().unwrap().execution_nonce,
        first.context.execution_nonce
    );
    c.excluded(&mut actor, "resume-owned");
    drop(actor);
    let mut actor = c.open();
    c.run(&mut actor).await;
    c.excluded(&mut actor, "resume-owned");
    c.input("resume-direct-twin", &frame);
    c.run(&mut actor).await;
    assert!(
        actor
            .writer
            .store()
            .ledger()
            .excluded(&input_key("resume-direct-twin"))
            .is_none()
    );
    assert_eq!(c.notices(), 1);
    assert_eq!(actor.notice_debt_extra(), 0);
}

#[tokio::test(start_paused = true)]
async fn notice_owner_resume_of_other_source_cannot_exclude_this_source() {
    let c = Case::new("other-source-resume");
    launch(&c, false, "native");
    let mut actor = c.open();
    c.run(&mut actor).await;
    launch(&c, true, "other-native");
    let frame = c.frame();
    register_current(&c, &frame);
    c.input("foreign-resume", &frame);
    c.run(&mut actor).await;
    assert!(
        actor
            .writer
            .store()
            .ledger()
            .excluded(&input_key("foreign-resume"))
            .is_none()
    );
    assert_eq!(c.notices(), 0);
    assert_eq!(actor.notice_debt_extra(), 1);
}

#[tokio::test(start_paused = true)]
async fn notice_owner_resume_context_change_invalidates_prepared_census() {
    use history::notice_owner::{Batch, Decision};
    let c = Case::new("resume-context-change");
    launch(&c, false, "native");
    let mut actor = c.open();
    c.run(&mut actor).await;
    let resumed = launch(&c, true, "native");
    let frame = c.frame();
    register_current(&c, &frame);
    c.input("context-change", &frame);
    crate::services::tui_o::writer::restart_end::refresh(
        &mut actor.writer,
        &mut actor.deriver,
        actor.bindings.as_ref(),
        ShadowProvider::Claude,
    )
    .await
    .unwrap();
    actor.read_sources();
    let origin = actor
        .writer
        .store()
        .ledger()
        .input_notices()
        .evidence
        .values()
        .find(|e| e.unit_key.as_ref() == Some(&input_key("context-change")))
        .unwrap()
        .origin
        .clone()
        .unwrap();
    let batch = Batch::prepare(actor.writer.store());
    assert_eq!(
        batch.decision(&origin, "context-change", &frame),
        Decision::Owned
    );
    std::fs::remove_file(resumed.path).unwrap();
    assert_eq!(
        batch.decision(&origin, "context-change", &frame),
        Decision::Pending("execution_context_changed")
    );
    c.run(&mut actor).await;
    assert!(
        actor
            .writer
            .store()
            .ledger()
            .excluded(&input_key("context-change"))
            .is_none()
    );
    assert_eq!(c.notices(), 0);
    assert_eq!(actor.notice_debt_extra(), 1);
}
