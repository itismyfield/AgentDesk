//! A thread child's store retirement from the real thread GC boundary to its next input.

use super::thread_child_tests::{booted, install, stored};
use super::*;
use crate::services::dispatched_sessions::hosted_cleanup::thread_gc::{
    ChildStore, gc_stale_thread_sessions_retiring_pg,
};
use crate::services::platform::tmux::SessionPresence;

async fn stale_thread_row(pool: &sqlx::PgPool, key: &str, child: u64) {
    sqlx::query(
        "INSERT INTO sessions (
             session_key, provider, identity_kind, discord_token_hash, channel_id,
             status, thread_channel_id, last_heartbeat
         ) VALUES ($1, 'claude', 'discord_channel', 'retirement-fixture', $1,
                   'idle', $2, NOW() - INTERVAL '2 hours')",
    )
    .bind(key)
    .bind(child.to_string())
    .execute(pool)
    .await
    .unwrap();
}

async fn refresh(shared: &Arc<SharedData>) {
    super::super::super::super::owner_cleanup::refresh_writers(
        shared,
        &ProviderKind::Claude,
        Some(&TuiOConfig::default()),
    )
    .await;
}

/// Runs the refresh step, then waits for the child's writer to take work.
async fn hosted(shared: &Arc<SharedData>, child: u64) -> bool {
    refresh(shared).await;
    let accepts = async {
        while !host::channel_accepts(child) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(12), accepts)
        .await
        .is_ok()
}

#[tokio::test]
async fn thread_child_store_retires_only_after_its_rows_gc_delete_and_its_next_input_resumes_it_pg()
{
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::thread_child_store_retires_only_after_its_rows_gc_delete_and_its_next_input_resumes_it_pg"
    )) {
        return;
    }
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    use crate::services::tui_o::cutover::{BodyClaim, BodySend, BodySite, claim_then_send_at};
    let root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (parent, child) = (73_369_631, 733_696_320_000_001_u64);
    booted(root.path(), parent, child);
    let (_registry, shared) = runtime(&pool, ProviderKind::Claude, parent, "gateway").await;
    let state = discord_mock::DiscordMockState::new();
    state.allow_channel(parent);
    state.allow_channel(child);
    let (proxy, gateway, _server) = discord_mock::start(state).await;
    let context = discord_mock::serenity_context(proxy, gateway).await;
    assert!(shared.http.cached_serenity_ctx.set(context).is_ok());
    crate::services::tui_o::ownership::gate("claude").acquired();
    super::super::super::super::owner_execution::prepare(&ProviderKind::Claude, Some(&pool)).await;
    assert!(hosted(&shared, child).await);
    let ready = host::process_readiness();
    let mut first = ready.hosted_by(child).unwrap();
    let parent_writer = ready.hosted_by(parent).unwrap();
    let child_dir = root.path().join("o_store").join(child.to_string());
    let mark = || -> Option<Value> {
        let bytes = std::fs::read(child_dir.join("child_lifecycle")).ok()?;
        Some(serde_json::from_slice(&bytes).unwrap())
    };
    let init = std::fs::read(child_dir.join("init")).unwrap();
    let key = format!("test-host:AgentDesk-claude-retire-t{child}");
    stale_thread_row(&pool, &key, child).await;
    let children = host::ChildStores;

    // A delete the SQL recheck refuses asks for nothing, though the probe read Missing.
    let heartbeat = |hours: i32| {
        let pool = pool.clone();
        let sql = "UPDATE sessions SET last_heartbeat = NOW() - make_interval(hours => $1)";
        async move { sqlx::query(sql).bind(hours).execute(&pool).await.unwrap() }
    };
    let touched = || async {
        heartbeat(0).await;
        SessionPresence::Missing
    };
    let kept = gc_stale_thread_sessions_retiring_pg(&pool, |_| touched(), &children).await;
    assert!(kept.is_empty());
    assert_eq!((ready.hosted_by(child), mark()), (Some(first), None));
    heartbeat(2).await;

    // A request pinned before an input is refused after it: the writer and the store stay.
    let pinned = children.pin(child).expect("an active hosted child pins");
    assert_eq!(
        for_input_placement("claude", child).await,
        IntakeRoute::Gateway
    );
    let late = children.retire(pinned, Box::pin(async { Ok(false) })).await;
    assert!(late.is_err(), "{late:?}");
    assert_eq!((ready.hosted_by(child), mark()), (Some(first), None));
    assert!(host::channel_accepts(child));

    // A successor row, or no answer about one, keeps the store; its writer is hosted again.
    for answer in [Ok(true), Err("unreadable".to_owned())] {
        let pinned = children.pin(child).unwrap();
        let refused = children.retire(pinned, Box::pin(async { answer })).await;
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(mark(), None);
        assert!(hosted(&shared, child).await);
        assert_ne!(ready.hosted_by(child), Some(first));
        first = ready.hosted_by(child).unwrap();
    }

    // The exact delete retires the store: that writer generation settled, then the durable mark.
    let deleted = gc_stale_thread_sessions_retiring_pg(
        &pool,
        |_| async { SessionPresence::Missing },
        &children,
    )
    .await;
    assert_eq!(deleted, vec![key]);
    assert_eq!(ready.hosted_by(child), None, "the child writer was joined");
    assert_eq!(
        mark(),
        Some(json!({"schema":1,"generation":1,"retired":true}))
    );
    assert_eq!(std::fs::read(child_dir.join("init")).unwrap(), init);
    assert_eq!(ready.hosted_by(parent), Some(parent_writer));
    assert!(host::channel_accepts(parent));
    assert!(
        !root
            .path()
            .join("o_store")
            .join(parent.to_string())
            .join("child_lifecycle")
            .exists()
    );
    // Retired is neither hosted again nor handed to Legacy.
    refresh(&shared).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!ready.is_hosted(child));
    assert!(matches!(
        crate::services::tui_o::cutover::intake_route::route("claude", child),
        IntakeRoute::Hold(_)
    ));
    let mut sends = 0;
    let claim = Some(BodyClaim::new(child, Some(ClaudeTui)));
    let sent = claim_then_send_at(BodySite::WatcherTerminal, claim, || async { sends += 1 }).await;
    assert!(matches!(sent, Ok(BodySend::OwnedByO)), "{sent:?}");
    assert_eq!(sends, 0);

    let candidate = || {
        let owned = cutover::boot_ownership();
        let found = owned.into_iter().find(|(id, _, _)| *id == child);
        found.is_some_and(|(_, _, candidate)| candidate.is_some())
    };
    assert!(!candidate());

    // The next input supersedes the mark over the same store and a new writer takes it.
    assert!(matches!(
        for_input_placement("claude", child).await,
        IntakeRoute::Hold(_)
    ));
    assert_eq!(
        mark(),
        Some(json!({"schema":1,"generation":2,"retired":false}))
    );
    assert!(candidate());
    assert!(hosted(&shared, child).await);
    let second = ready.hosted_by(child).unwrap();
    assert_ne!(second, first);
    assert_eq!(std::fs::read(child_dir.join("init")).unwrap(), init);
    assert_eq!(
        for_input_placement("claude", child).await,
        IntakeRoute::Gateway
    );
    pool.close().await;
}

#[tokio::test]
async fn thread_child_retired_before_a_restart_boots_with_no_writer_candidate_until_its_next_input()
{
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::thread_child_retired_before_a_restart_boots_with_no_writer_candidate_until_its_next_input"
    )) {
        return;
    }
    let root = crate::config::TestRuntimeRootGuard::new();
    let (parent, child) = (73_369_641, 733_696_420_000_001_u64);
    stored(root.path(), parent, child);
    let mark = root
        .path()
        .join("o_store")
        .join(child.to_string())
        .join("child_lifecycle");
    std::fs::write(&mark, br#"{"schema":1,"generation":3,"retired":true}"#).unwrap();
    install(parent);
    let candidate = |channel: u64| {
        let owned = cutover::boot_ownership();
        let found = owned.into_iter().find(|(id, _, _)| *id == channel);
        found.map(|(_, _, candidate)| candidate.is_some())
    };
    // The child is still known to the boot, but only the parent gets a writer.
    assert_eq!(
        (candidate(parent), candidate(child)),
        (Some(true), Some(false))
    );
    assert!(matches!(
        for_input_placement("claude", child).await,
        IntakeRoute::Hold(_)
    ));
    let read: Value = serde_json::from_slice(&std::fs::read(&mark).unwrap()).unwrap();
    assert_eq!(read, json!({"schema":1,"generation":4,"retired":false}));
    assert_eq!(candidate(child), Some(true));
}

#[tokio::test]
async fn thread_child_writer_owing_output_refuses_retirement_and_keeps_writer_and_store_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::thread_child_writer_owing_output_refuses_retirement_and_keeps_writer_and_store_pg"
    )) {
        return;
    }
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    use crate::services::tui_o::writer::actor::{Demand, Undelivered};
    let root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (parent, child) = (73_369_651, 733_696_520_000_001_u64);
    booted(root.path(), parent, child);
    let (_registry, shared) = runtime(&pool, ProviderKind::Claude, parent, "gateway").await;
    let state = discord_mock::DiscordMockState::new();
    state.allow_channel(parent);
    state.allow_channel(child);
    let (proxy, gateway, _server) = discord_mock::start(state).await;
    let context = discord_mock::serenity_context(proxy, gateway).await;
    assert!(shared.http.cached_serenity_ctx.set(context).is_ok());
    crate::services::tui_o::ownership::gate("claude").acquired();
    super::super::super::super::owner_execution::prepare(&ProviderKind::Claude, Some(&pool)).await;
    assert!(hosted(&shared, child).await);
    let ready = host::process_readiness();
    let writer = ready.hosted_by(child).unwrap();
    let child_dir = root.path().join("o_store").join(child.to_string());
    let mark = || std::fs::read(child_dir.join("child_lifecycle")).ok();
    let children = host::ChildStores;

    // The child's fresh owed read answers one unposted item for every ask after it.
    let real = ready
        .undelivered(child)
        .expect("the child writer publishes what it owes");
    let (owes, published) = tokio::sync::watch::channel(None::<Undelivered>);
    let demand = Demand::default();
    let asks = demand.clone();
    let answering = tokio::spawn(async move {
        loop {
            if asks.wanted() {
                let owed = Undelivered {
                    owed: 1,
                    asked: asks.asked(),
                    ..Undelivered::default()
                };
                owes.send_replace(Some(owed));
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    ready.track_owed_for_test(child, host::OwedView { published, demand });
    let pinned = children.pin(child).expect("an active hosted child pins");
    let refused = children.retire(pinned, Box::pin(async { Ok(false) })).await;
    answering.abort();
    let reason = refused.expect_err("an unposted item refuses the retirement");
    assert!(reason.contains("still owes output"), "{reason}");
    assert_eq!((ready.hosted_by(child), mark()), (Some(writer), None));
    let candidate = cutover::boot_ownership()
        .into_iter()
        .find(|(id, _, _)| *id == child)
        .and_then(|(_, _, candidate)| candidate);
    assert!(
        candidate.is_some(),
        "the refused store stays a writer candidate"
    );

    // With nothing owed the same request retires that writer generation.
    ready.track_owed_for_test(child, real);
    let pinned = children
        .pin(child)
        .expect("the refusal kept the child active");
    let retired = children.retire(pinned, Box::pin(async { Ok(false) })).await;
    assert!(retired.is_ok(), "{retired:?}");
    assert_eq!(ready.hosted_by(child), None);
    let read: Value = serde_json::from_slice(&mark().unwrap()).unwrap();
    assert_eq!(read, json!({"schema":1,"generation":1,"retired":true}));
    pool.close().await;
}
