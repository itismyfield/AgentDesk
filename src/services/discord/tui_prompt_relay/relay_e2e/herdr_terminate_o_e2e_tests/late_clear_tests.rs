#[path = "record_boundary_tests.rs"]
mod record_boundary_tests;

use super::*;
use crate::services::discord::commands::control::ClearReport;
use crate::services::discord::commands::control::clear_channel_session_state_with_session_key;
use crate::services::discord::recovery_engine::host_reconcile::HostReconcile;

#[test]
fn late_source_bind_after_clear_cannot_revive_session_pg() {
    let path = concat!(
        module_path!(),
        "::late_source_bind_after_clear_cannot_revive_session_pg"
    );
    in_child(path, |rig, _| Box::pin(late_source(rig)));
}

async fn selectors(run: &Terminating) -> (Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT claude_session_id, raw_provider_session_id FROM sessions WHERE session_key = $1",
    )
    .bind(&run.session_key)
    .fetch_one(&run.pool)
    .await
    .unwrap()
}

async fn seed_selectors(run: &Terminating, native: &str) {
    sqlx::query("UPDATE sessions SET claude_session_id = $2, raw_provider_session_id = $2 WHERE session_key = $1")
        .bind(&run.session_key)
        .bind(native)
        .execute(&run.pool)
        .await
        .unwrap();
    assert_eq!(
        selectors(run).await,
        (Some(native.to_owned()), Some(native.to_owned()))
    );
}

async fn clear_exact(run: &Terminating, rig: &herdr::P11TerminationRig) -> ClearReport {
    assert!(rig.writes().is_empty(), "no host write before clear");
    let http = Arc::new(poise::serenity_prelude::Http::new(""));
    let cleared = clear_channel_session_state_with_session_key(
        &http,
        &run.harness.shared,
        &ProviderKind::Codex,
        run.harness.channel_id,
        "!clear",
        Some(&run.session_key),
    )
    .await
    .expect("the actual channel clear completes");
    let retired = Some((herdr::HostedState::Retired, herdr::EXECUTION.to_string()));
    assert_eq!(
        herdr::row(&run.pool, &run.session_key).await,
        retired,
        "{cleared:?}"
    );
    assert_eq!(
        rig.writes(),
        [(
            herdr::SINGLE_PANE_TERMINATION.to_owned(),
            run.location.pane_id.clone(),
        )],
        "clear must close exactly the old pane"
    );
    assert_eq!(selectors(run).await, (None, None), "{cleared:?}");
    let core = run.harness.shared.core.lock().await;
    let session = &core.sessions[&run.harness.channel_id];
    assert!(
        session.cleared && session.session_id.is_none(),
        "{cleared:?}"
    );
    cleared
}

async fn late_source(rig: herdr::P11TerminationRig) {
    let (run, context, sessions) = Terminating::launch_codex(&rig).await;
    let native = uuid::Uuid::new_v4().to_string();
    let path = rollout(&sessions, &native);
    seed_selectors(&run, &native).await;
    assert!(
        binding_events::binding_events_since(CHANNEL_ID, 0)
            .unwrap()
            .is_empty(),
        "the launched execution has not received its first Startup yet"
    );

    clear_exact(&run, &rig).await;
    let retired = Some((herdr::HostedState::Retired, herdr::EXECUTION.to_string()));

    // The delayed hook is real ingress; the attach must still reject the ended incarnation.
    let late = session_start(&run.logical, &context, &native, &path);
    let source = source_id_for(&native, &path).expect("the late hook names an existing rollout");
    let reader = run.own_pane(&native);
    let attached = attach_launched_codex_herdr_source(
        &run.pool,
        &run.owner,
        CHANNEL_ID,
        herdr::EXECUTION,
        &source,
        &reader,
    )
    .await;
    assert_eq!(
        attached,
        HerdrSourceAttach::Refused(HostReconcile::Unresolved("retired hosted record".into())),
        "late ingress: {late:?}"
    );
    assert_eq!(herdr::row(&run.pool, &run.session_key).await, retired);
    assert_eq!(selectors(&run).await, (None, None));
    run.finish().await;
}
