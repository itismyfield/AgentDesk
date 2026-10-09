//! Delegated homes as each provider runtime starts them: the switch, the role's restore and the
//! standby writer, read through the boot entries.

use super::*;
use crate::db::o_channel_homes::{self, HomeWrite};
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::cluster::channel_home::{self, HomeOwnership};
use crate::services::cluster::channel_home_boot;
use crate::services::cluster::channel_home_drain::{DrainPort, ResetRefused};
use crate::services::cluster::channel_home_port::ChannelHomePort;
use crate::services::tui_o::channel_policy::Adoption;
use crate::services::tui_o::cutover::test_override;
use crate::services::tui_o::writer::DiscordPort;
use crate::services::tui_o::writer::host::HostIo;

/// The turn a delegated drain would read for `channel` through `restored`.
async fn turn(restored: Arc<std::sync::atomic::AtomicBool>) -> Option<bool> {
    ChannelHomePort::new(9_300_000_000_000_001, Arc::default(), restored)
        .turn_running()
        .await
}

fn unrestored() -> Arc<SharedData> {
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared.restart.reconcile_done.store(false, Ordering::SeqCst);
    shared
}

/// Turns count as restored after a gateway's recovery or a REST worker's own queue restore; a
/// utility gateway and a standby mark reconcile done unrestored, so theirs stay unknown.
#[tokio::test]
async fn turns_count_as_restored_only_where_the_role_restored_them() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let gateway = unrestored();
    let flag = turns_restored(&gateway, HomeRole::Gateway).await;
    assert_eq!(turn(Arc::clone(&flag)).await, None, "before recovery");
    mark_reconcile_complete(&gateway);
    assert_eq!(turn(flag).await, Some(false), "after recovery");

    let utility = unrestored();
    utility.settings.write().await.agent = Some("spark".into());
    let flag = turns_restored(&utility, HomeRole::Gateway).await;
    mark_reconcile_complete(&utility);
    assert_eq!(turn(flag).await, None, "a utility gateway skips recovery");

    let standby = unrestored();
    let flag = turns_restored(&standby, HomeRole::Standby).await;
    let registry = Arc::new(health::HealthRegistry::new());
    register_standby_and_settle_reconcile(&registry, &ProviderKind::Claude, &standby).await;
    assert!(standby.restart.reconcile_done.load(Ordering::SeqCst));
    assert_eq!(turn(flag).await, None, "a standby restores nothing");

    let worker = unrestored();
    let flag = turns_restored(&worker, HomeRole::RestWorker).await;
    queued_recovery::restore_worker_queues(&worker, &ProviderKind::Claude).await;
    assert_eq!(
        turn(Arc::clone(&flag)).await,
        None,
        "no REST client: not run"
    );
    let (_log, _rest) = super::super::shared_state::test_rest::recording_mock(1, 1).await;
    queued_recovery::restore_worker_queues(&worker, &ProviderKind::Claude).await;
    assert_eq!(turn(flag).await, Some(false), "after the queue restore");
}

fn settings(switch: Option<bool>) -> HomeSettings {
    HomeSettings {
        switch,
        instance_id: Some("mini".into()),
    }
}

/// Off or unset, the entry returns before any read: the lazy pool never opens a connection and
/// no gate, task or writer exists. On with no row naming this node, one read and nothing more.
#[tokio::test]
async fn the_entry_reads_nothing_off_and_starts_nothing_without_a_row_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    db.connect_and_migrate().await.close().await;
    let lazy = sqlx::postgres::PgPoolOptions::new().connect_lazy(&db.database_url);
    let lazy = lazy.expect("lazy pool");
    let shared = super::super::make_shared_data_for_tests_with_storage(Some(lazy.clone()));
    for switch in [None, Some(false)] {
        let settings_switch = settings(switch);
        let started = start_channel_homes(
            &shared,
            &ProviderKind::Claude,
            HomeRole::Standby,
            &settings_switch,
        );
        assert!(started.await.is_empty());
        assert!(o_writer_host::spawn_delegated(&shared, &ProviderKind::Claude, None).is_empty());
    }
    assert_eq!(lazy.size(), 0, "off: no connection opened");
    assert!(!channel_home::any_registered());

    let settings_on = settings(Some(true));
    let on = start_channel_homes(
        &shared,
        &ProviderKind::Claude,
        HomeRole::Standby,
        &settings_on,
    );
    assert!(on.await.is_empty());
    assert!(lazy.size() >= 1, "on: the rows were read");
    assert!(!channel_home::any_registered());
    assert!(o_writer_host::spawn_delegated(&shared, &ProviderKind::Claude, None).is_empty());
    lazy.close().await;
    db.drop().await;
}

/// On a standby holding a worker row: one gate registers and opens by its lease's write, and only
/// that channel gets a writer, off or on the O home, while other selected channels get none.
#[tokio::test]
async fn a_standby_holder_registers_renews_and_hosts_only_its_delegated_writer_pg() {
    const HELD: u64 = 9_300_000_000_000_003;
    const PLAIN: u64 = 9_300_000_000_000_004;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = HELD.to_string();
    let applied = |write: Result<HomeWrite<o_channel_homes::ChannelHome>, _>| match write {
        Ok(HomeWrite::Applied(row)) => row,
        other => panic!("{other:?}"),
    };
    let row = applied(o_channel_homes::delegate(&pool, &channel, "claude", "gw", "mini").await);
    let row = applied(o_channel_homes::finish_release(&pool, &channel, "gw", row.epoch).await);
    let epoch = applied(o_channel_homes::adopt(&pool, &channel, "mini", row.epoch).await).epoch;
    let shared = super::super::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let _hosts = crate::config::session_hosts::force_for_test(
        Some("mini"),
        &[(HELD, "mini"), (PLAIN, "mini")],
    );
    let (_log, _rest) = super::super::shared_state::test_rest::recording_mock(1, HELD).await;

    let settings_on = settings(Some(true));
    let started = start_channel_homes(
        &shared,
        &ProviderKind::Claude,
        HomeRole::Standby,
        &settings_on,
    );
    let started = started.await;
    assert_eq!(started.len(), 1);
    let mut opened = false;
    for _ in 0..250 {
        if let HomeOwnership::Owned { home_epoch, .. } = started[0].ownership() {
            opened = home_epoch == epoch;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(opened, "opened by the lease's renewal");
    assert_eq!(channel_home_boot::running(&channel), (true, Some(epoch)));

    let channels = [(HELD, ClaudeTui), (PLAIN, ClaudeTui)];
    let foreign = test_override::force_standby(&channels, "gw", Adoption::Committed);
    let writers = o_writer_host::spawn_delegated(&shared, &ProviderKind::Claude, None);
    assert_eq!(
        writers.len(),
        1,
        "off the O home: only the delegated channel"
    );
    writers.iter().for_each(tokio::task::JoinHandle::abort);
    drop(foreign);

    // On the O home every selected channel has an adoption; still only a registered one is hosted.
    const HOME_HELD: u64 = 9_300_000_000_000_008;
    const HOME_PLAIN: u64 = 9_300_000_000_000_009;
    let _hosts = crate::config::session_hosts::force_for_test(
        Some("mini"),
        &[(HOME_HELD, "mini"), (HOME_PLAIN, "mini")],
    );
    let _home = test_override::force_channels(&[(HOME_HELD, ClaudeTui), (HOME_PLAIN, ClaudeTui)]);
    channel_home::unregister(&channel);
    let home_held = HOME_HELD.to_string();
    channel_home::register(Arc::new(channel_home::HomeGate::new(&home_held, "mini")));
    let writers = o_writer_host::spawn_delegated(&shared, &ProviderKind::Claude, None);
    assert_eq!(
        writers.len(),
        1,
        "on the O home: only the registered channel"
    );
    writers.iter().for_each(tokio::task::JoinHandle::abort);
    channel_home::unregister(&home_held);
    let alarmed = crate::services::tui_o::alarm::health_reasons();
    let unregistered = [PLAIN, HOME_PLAIN].map(|channel| format!(":{channel}"));
    let alarmed = alarmed
        .iter()
        .filter(|r| unregistered.iter().any(|c| r.ends_with(c)));
    assert_eq!(
        alarmed.count(),
        0,
        "no hold alarm for an unregistered channel"
    );
    channel_home_boot::stop(&channel).await;
    channel_home::unregister(&channel);
    pool.close().await;
    db.drop().await;
}

/// The standby writer's port names its bot over the REST client and posts through it.
#[cfg(unix)]
#[tokio::test]
async fn a_standby_writers_port_posts_over_the_bot_tokens_rest_client() {
    use crate::services::discord::admin_host_guard::tests::Recorder;
    let me = serde_json::json!({"id": "4242", "username": "bot", "discriminator": "0001",
        "avatar": null, "bot": true});
    let answer =
        move |_: &axum::http::Method, path: &str| path.ends_with("/users/@me").then(|| me.clone());
    let discord = Recorder::start_with(Arc::new(answer)).await;
    let shared = crate::services::discord::make_shared_data_for_tests();
    let host = o_writer_host::test_host::rest(&shared, Arc::clone(&discord.http));
    let port = host.port().await;
    assert_eq!(port.bot_id(), 4242, "named by the REST client");
    let posted = port.post(77, "piece".into()).await;
    assert!(matches!(
        posted,
        crate::services::tui_o::writer::PostOutcome::Created(_)
    ));
    let calls = discord.take();
    assert!(
        calls[0].starts_with("GET ") && calls[0].contains("/users/@me"),
        "{calls:?}"
    );
    assert!(
        calls[1].starts_with("POST ") && calls[1].contains("/channels/77/messages"),
        "{calls:?}"
    );
}

/// A releasing drain's reset runs main's managed reset on a Legacy session, keeps a pane whose
/// Herdr endpoint runs here untouched, and reports a refusal of the existing path.
#[cfg(unix)]
#[tokio::test]
async fn the_legacy_reset_runs_the_existing_reset_and_keeps_a_local_pane_pg() {
    use crate::services::discord::host_defer_gate::tests::{ScriptedTmux, map_channel, postgres};
    const LEGACY: u64 = 9_300_000_000_000_005;
    const PANE: u64 = 9_300_000_000_000_006;
    const ELSEWHERE: u64 = 9_300_000_000_000_007;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let shared = crate::services::discord::host_teardown_gate::test_support::shared_on(&pool).await;
    let (_log, _rest) = super::super::shared_state::test_rest::recording_mock(1, LEGACY).await;
    let _hosts = crate::config::session_hosts::force_for_test(
        Some("mini"),
        &[(PANE, "mini"), (ELSEWHERE, "book")],
    );
    for channel in [LEGACY, PANE, ELSEWHERE] {
        map_channel(
            &shared,
            ChannelId::new(channel),
            &format!("home-reset-{channel}"),
        )
        .await;
        let mut core = shared.core.lock().await;
        core.sessions
            .get_mut(&ChannelId::new(channel))
            .unwrap()
            .session_id = Some("sid".into());
    }
    let session = |channel| {
        let shared = Arc::clone(&shared);
        async move {
            shared.core.lock().await.sessions[&ChannelId::new(channel)]
                .session_id
                .clone()
        }
    };
    tmux.take_calls();

    assert_eq!(
        legacy_reset(&shared, &ProviderKind::Claude, PANE)().await,
        Ok(())
    );
    assert_eq!(
        session(PANE).await.as_deref(),
        Some("sid"),
        "the pane's session is kept"
    );
    assert_eq!(tmux.take_calls(), Vec::<String>::new(), "nothing touched");

    let refused = legacy_reset(&shared, &ProviderKind::Claude, ELSEWHERE)().await;
    assert!(
        matches!(refused, Err(ResetRefused::Refused(_))),
        "{refused:?}"
    );
    assert_eq!(session(ELSEWHERE).await.as_deref(), Some("sid"));

    assert_eq!(
        legacy_reset(&shared, &ProviderKind::Claude, LEGACY)().await,
        Ok(())
    );
    assert_eq!(
        session(LEGACY).await,
        None,
        "main's reset cleared the provider session"
    );
    let kills = tmux
        .take_calls()
        .into_iter()
        .filter(|call| call.starts_with("kill-session"));
    assert_eq!(kills.count(), 0, "no tmux session to end here");
    pool.close().await;
    db.drop().await;
}

/// Switched on without a PG pool or instance id, every selected Herdr channel takes no turn and gets
/// no writer while a plain one keeps the old path; off or unset, every channel keeps it.
#[tokio::test]
async fn a_switched_on_runtime_it_cannot_prepare_holds_its_herdr_channels() {
    use crate::services::cluster::home_availability::{self, Availability, Unavailable};
    use crate::services::tui_o::shadow::ShadowProvider;
    use crate::services::tui_o::writer::host::{self, HostParts, test_io::TestHost};
    use crate::services::turn_host::{HerdrRefusal, refusal_before_turn};
    const HERDR: u64 = 9_300_000_000_000_011;
    const PLAIN: u64 = 9_300_000_000_000_012;
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(HERDR, "gw")]);
    let _selected = test_override::force_candidates(&[(HERDR, ClaudeTui), (PLAIN, ClaudeTui)]);
    let turn =
        |channel| refusal_before_turn(None, &ProviderKind::Claude, channel, || async { None });
    let unavailable = |refusal: Option<HerdrRefusal>| {
        matches!(refusal, Some(HerdrRefusal::DelegationUnavailable { .. }))
    };
    let root = tempfile::tempdir().unwrap();
    let writers = || {
        let io = TestHost::new([]);
        let alarms = io.alarms.clone();
        let parts = || HostParts {
            io,
            runtime_root: Some(root.path().to_path_buf()),
            gate: Arc::default(),
            readiness: Arc::default(),
        };
        let started = host::start(ShadowProvider::Claude, true, parts);
        started.iter().for_each(tokio::task::JoinHandle::abort);
        let alarmed = alarms.0.lock().unwrap();
        let held = alarmed.iter().filter(|(channel, _)| *channel == HERDR);
        (started.len(), held.count())
    };

    for switch in [None, Some(false)] {
        let unset = HomeSettings {
            switch,
            instance_id: None,
        };
        install_home_availability(&ProviderKind::Claude, &unset, false);
        assert_eq!(home_availability::state("claude"), Availability::Off);
        assert!(!unavailable(turn(HERDR).await), "off: the old judgement");
    }
    assert_eq!(writers(), (2, 0), "off: both channels hosted");

    let cases = [
        (Some("mini"), false, Unavailable::MissingPool),
        (None, true, Unavailable::MissingInstanceId),
    ];
    for (instance_id, has_pool, reason) in cases {
        let on = HomeSettings {
            switch: Some(true),
            instance_id: instance_id.map(str::to_owned),
        };
        install_home_availability(&ProviderKind::Claude, &on, has_pool);
        assert_eq!(
            home_availability::state("claude"),
            Availability::Unavailable(reason)
        );
        let refused = HerdrRefusal::DelegationUnavailable { reason };
        assert_eq!(turn(HERDR).await, Some(refused));
        assert_eq!(
            turn(PLAIN).await,
            None,
            "a plain channel keeps the old path"
        );
        assert_eq!(
            writers(),
            (1, 1),
            "the Herdr channel is held with an alarm, not hosted"
        );
    }

    install_home_availability(&ProviderKind::Claude, &settings(Some(true)), true);
    assert_eq!(home_availability::state("claude"), Availability::Ready);
    assert!(!unavailable(turn(HERDR).await));
    assert_eq!(writers(), (2, 0));
}
