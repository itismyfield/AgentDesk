use super::*;
use crate::services::tui_o::store::codex_end;
use crate::services::tui_prompt_dedupe::{binding_context, hosted_source};

#[test]
fn codex_herdr_clear_with_exact_source_exposes_record_gap_pg() {
    in_child(
        concat!(
            module_path!(),
            "::codex_herdr_clear_with_exact_source_exposes_record_gap_pg"
        ),
        |rig, _| Box::pin(clear_then_launch(rig, false)),
    );
}

#[test]
fn codex_herdr_clear_with_lost_source_exposes_warning_gap_pg() {
    in_child(
        concat!(
            module_path!(),
            "::codex_herdr_clear_with_lost_source_exposes_warning_gap_pg"
        ),
        |rig, _| Box::pin(clear_then_launch(rig, true)),
    );
}

async fn clear_then_launch(rig: herdr::P11TerminationRig, lose_source: bool) {
    let (run, context, sessions) = Terminating::launch_codex(&rig).await;
    let old_session = uuid::Uuid::new_v4().to_string();
    let old_path = rollout(&sessions, &old_session);
    let old_source = own_source(&run, &context, &old_session, &old_path).await;
    let exact_source = || {
        hosted_source::capture(
            "codex",
            &run.logical,
            CHANNEL_ID,
            &old_path,
            Some(&old_session),
        )
    };
    let pin = exact_source().expect("the bound execution has a readable strict source");
    assert_eq!(pin.execution_nonce, herdr::EXECUTION);
    assert_eq!(pin.source, old_source);
    let prepared = binding_context::execution_context("codex", herdr::EXECUTION).unwrap();
    assert_eq!(
        prepared.host.as_deref(),
        Some(run.location.execution_node.as_str())
    );
    seed_selectors(&run, &old_session).await;
    if lose_source {
        std::fs::rename(&old_path, old_path.with_extension("unavailable")).unwrap();
        assert!(
            exact_source().is_none(),
            "the bound native source became unavailable"
        );
    }
    assert!(codex_end::read(&root(), CHANNEL_ID).unwrap().is_empty());

    let cleared = clear_exact(&run, &rig).await;
    launch_fresh(&run, &sessions).await;

    // These assertions capture the current gap at the clear's durable record and reply.
    let records = codex_end::read(&root(), CHANNEL_ID).unwrap();
    assert!(
        records.is_empty(),
        "the old execution currently has no End: {records:?}"
    );
    if lose_source {
        assert_eq!(
            cleared.warning(),
            None,
            "the lost source currently has no warning"
        );
    }
    run.finish().await;
}

async fn launch_fresh(run: &Terminating, sessions: &Path) {
    let (selector, _) = selectors(run).await;
    let mut launch = run.launch(selector.clone(), Some(sessions.to_path_buf()));
    launch.resume = selector.is_some();
    let cwd = root().join("fresh-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let prepare = |_: &PreparedIncarnation| {
        Ok(HerdrLaunchCommand {
            cwd,
            command: "codex".into(),
        })
    };
    let result =
        launch_herdr_session(&run.pool, launch, prepare, Arc::new(Successor("codex"))).await;
    let Ok(HerdrLaunchOutcome::Launched {
        execution_nonce,
        evidence: Ok(()),
        ..
    }) = result
    else {
        panic!("the first launch after retirement must proceed: {result:?}");
    };
    assert_ne!(execution_nonce, herdr::EXECUTION);
    let context = binding_context::execution_context("codex", &execution_nonce).unwrap();
    assert_eq!(context.launch_mode, "fresh");
    assert_eq!(context.expected_native_session_id, None);
    assert_eq!(
        herdr::row(&run.pool, &run.session_key).await,
        Some((herdr::HostedState::Pending, execution_nonce))
    );
}
