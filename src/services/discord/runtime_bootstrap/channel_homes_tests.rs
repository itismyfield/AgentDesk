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
use crate::services::tui_prompt_dedupe::binding_events;

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
        assert!(
            o_writer_host::lifecycles(None).is_empty(),
            "off: no shutdown lifecycle"
        );
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
    use crate::services::cluster::home_supervisor::{HomeLifecycle, StopReason};
    let homes = o_writer_host::home_writers(&ProviderKind::Claude, true);
    homes.stop_and_join(StopReason::Sigterm).await;
    assert_eq!(channel_home_boot::running(&channel), (false, None));
    channel_home::unregister(&channel);
    pool.close().await;
    db.drop().await;
}

/// A REST worker targeted by a released row adopts it through its own port only once restored
/// and with a writer under its home; until then the home says what it waits on.
#[tokio::test]
async fn a_rest_worker_adopts_its_released_home_only_once_its_turns_are_restored_pg() {
    const RELEASED: u64 = 9_300_000_000_000_014;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = RELEASED.to_string();
    let applied = |write: Result<HomeWrite<o_channel_homes::ChannelHome>, _>| match write {
        Ok(HomeWrite::Applied(row)) => row,
        other => panic!("{other:?}"),
    };
    let row = applied(o_channel_homes::delegate(&pool, &channel, "claude", "gw", "mini").await);
    let released = applied(o_channel_homes::finish_release(&pool, &channel, "gw", row.epoch).await);
    let shared = super::super::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    shared.restart.reconcile_done.store(false, Ordering::SeqCst);
    let settings_on = settings(Some(true));
    let started = start_channel_homes(
        &shared,
        &ProviderKind::Claude,
        HomeRole::RestWorker,
        &settings_on,
    );
    let started = started.await;
    assert_eq!(started.len(), 1, "the released row names this node");
    // The watch runs on this test's thread; each round it finishes is one unrestored judgement.
    let judged = channel_home_boot::rounds(RELEASED) + 2;
    for _ in 0..1_000 {
        if channel_home_boot::rounds(RELEASED) >= judged {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        channel_home_boot::rounds(RELEASED) >= judged,
        "the watch judged twice"
    );
    let read = || o_channel_homes::read_home(&pool, &channel);
    assert_eq!(
        read().await.unwrap(),
        Some(released.clone()),
        "adopted unrestored"
    );
    assert_eq!(started[0].ownership(), HomeOwnership::Lost);

    mark_reconcile_complete(&shared);
    let judged = channel_home_boot::rounds(RELEASED) + 2;
    for _ in 0..1_000 {
        if channel_home_boot::rounds(RELEASED) >= judged {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        channel_home_boot::rounds(RELEASED) >= judged,
        "judged twice restored"
    );
    assert_eq!(
        read().await.unwrap(),
        Some(released.clone()),
        "adopted with no writer to run its first init"
    );
    assert_eq!(started[0].ownership(), HomeOwnership::Lost);
    let health = channel_home::health().expect("its home is registered");
    let waits = health["home_draining"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| {
            entry["channel"] == channel.as_str() && entry["blocker"] == "adopt_waits_writer"
        });
    assert!(waits, "{health}");
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
    const CODEX: u64 = 9_300_000_000_000_013;
    let _hosts =
        crate::config::session_hosts::force_for_test(Some("mini"), &[(HERDR, "gw"), (CODEX, "gw")]);
    let _selected = test_override::force_candidates(&[
        (HERDR, ClaudeTui),
        (PLAIN, ClaudeTui),
        (
            CODEX,
            crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
        ),
    ]);
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
        let _availability = install_home_availability(&ProviderKind::Claude, &unset, false);
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
        let _availability = install_home_availability(&ProviderKind::Claude, &on, has_pool);
        assert_eq!(
            home_availability::state("claude"),
            Availability::Unavailable(reason)
        );
        let refused = HerdrRefusal::DelegationUnavailable { reason };
        assert_eq!(turn(HERDR).await, Some(refused));
        assert_eq!(
            home_availability::refusal(CODEX),
            None,
            "the other provider is unaffected"
        );
        assert!(matches!(
            crate::services::tui_o::cutover::intake_route::route_for_placement("claude", HERDR),
            crate::services::tui_o::cutover::intake_route::IntakeRoute::Hold(_)
        ));
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

    let _availability =
        install_home_availability(&ProviderKind::Claude, &settings(Some(true)), true);
    assert_eq!(
        home_availability::state("claude"),
        Availability::PreflightPassed
    );
    assert!(!unavailable(turn(HERDR).await));
    assert_eq!(writers(), (2, 0));
}

/// What `channel`'s drain waits on, as health shows it.
fn blocker(channel: u64) -> Option<String> {
    let health = channel_home::health()?;
    let (draining, channel) = (
        health["home_draining"].as_array()?.iter(),
        channel.to_string(),
    );
    let mut named = draining.filter(|entry| entry["channel"].as_str() == Some(channel.as_str()));
    named.next()?["blocker"].as_str().map(str::to_owned)
}

/// Polls `channel`'s blocker until it reads `want`, recording each distinct value seen.
async fn blocker_reaches(channel: u64, want: &str, seen: &mut Vec<String>) -> bool {
    for _ in 0..1_000 {
        let now = blocker(channel);
        if let Some(now) = now.filter(|now| seen.last() != Some(now)) {
            seen.push(now);
        }
        if seen.last().map(String::as_str) == Some(want) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

/// Polls `channel`'s row until it leaves as reclaimed, recording each distinct blocker seen.
async fn reclaimed(pool: &sqlx::PgPool, channel: u64, seen: &mut Vec<String>) -> bool {
    for _ in 0..1_000 {
        if let Some(now) = blocker(channel).filter(|now| seen.last() != Some(now)) {
            seen.push(now);
        }
        let row = o_channel_homes::read_home(pool, &channel.to_string()).await;
        let row = row.unwrap().unwrap();
        if (row.state, row.holder.as_deref()) == (o_channel_homes::HomeState::Reclaimed, None) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

/// This thread's P5 binding-log root for a test, the previous one restored on drop.
struct P5Root(Option<std::path::PathBuf>);

impl P5Root {
    fn install(root: &std::path::Path) -> Self {
        let saved = binding_events::test_root();
        if !channel_home_boot::writer_mutant("p5_root_unset") {
            binding_events::set_test_root(Some(root));
        }
        Self(saved)
    }
}

impl Drop for P5Root {
    fn drop(&mut self) {
        binding_events::set_test_root(self.0.as_deref());
    }
}

/// A standby restarted as a reclaim's holder restores each channel's input before its drain reads
/// turns: a restored queue is busy until cleared, a failed restore stays unknown. Once cleared, a
/// channel this node never stored nor bound leaves; one whose session was bound here waits.
#[tokio::test]
async fn th6_standby_reclaim_restart_restores_turns_pg() {
    use crate::services::turn_orchestrator::{InterventionMode, save_channel_queue};
    const QUEUED: u64 = 9_300_000_000_000_021;
    const BROKEN: u64 = 9_300_000_000_000_022;
    const BOUND: u64 = 9_300_000_000_000_023;
    const ITEM: u64 = 9_300_000_000_000_031;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = crate::config::runtime_root().expect("runtime root");
    // The drain reads this thread's P5 root: its watch runs on this current-thread runtime.
    let flavor = tokio::runtime::Handle::current().runtime_flavor();
    assert_eq!(flavor, tokio::runtime::RuntimeFlavor::CurrentThread);
    let _p5 = P5Root::install(&root);
    let pending = binding_events::BindingTarget::Pending {
        payload_session_id: "s-bound".into(),
        payload_transcript_path: None,
    };
    let bound = binding_events::BindingEvent {
        seq: 1,
        channel_id: BOUND,
        provider: "claude".into(),
        tmux_session: "tmux-bound".into(),
        execution_nonce: None,
        old: None,
        new: pending,
        cause: binding_events::BindingCause::Startup,
        parent_hint: None,
        evidence: binding_events::BindingEvidence {
            hook_event: None,
            received_at: chrono::Utc::now(),
        },
        committed_at: chrono::Utc::now(),
    };
    let log = root.join(binding_events::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&log).unwrap();
    let mut line = serde_json::to_vec(&bound).unwrap();
    line.push(b'\n');
    std::fs::write(log.join(format!("{BOUND}.log")), line).unwrap();
    let config = crate::runtime_layout::config_file_path(&root);
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let agent = |id: u64| {
        let channel = format!("    channels:\n      claude:\n        id: \"{id}\"\n");
        format!("  - id: s3a-{id}\n    name: \"s3a {id}\"\n    provider: claude\n{channel}")
    };
    let agents = [QUEUED, BROKEN, BOUND].map(agent).concat();
    std::fs::write(config, format!("server:\n  port: 8791\nagents:\n{agents}")).unwrap();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let applied = |write: Result<HomeWrite<o_channel_homes::ChannelHome>, _>| match write {
        Ok(HomeWrite::Applied(row)) => row,
        other => panic!("{other:?}"),
    };
    for channel in [QUEUED, BROKEN, BOUND].map(|id| id.to_string()) {
        let row = applied(o_channel_homes::delegate(&pool, &channel, "claude", "gw", "mini").await);
        let row = applied(o_channel_homes::finish_release(&pool, &channel, "gw", row.epoch).await);
        let row = applied(o_channel_homes::adopt(&pool, &channel, "mini", row.epoch).await);
        applied(o_channel_homes::begin_reclaim(&pool, &channel, row.epoch, "gw").await);
    }
    let shared = super::super::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    shared.restart.reconcile_done.store(false, Ordering::SeqCst);
    let provider = ProviderKind::Claude;
    let item = Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(ITEM),
        queued_generation: 1,
        source_message_ids: vec![MessageId::new(ITEM)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "left by the last process".into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    };
    let queued = ChannelId::new(QUEUED);
    save_channel_queue(&provider, &shared.token_hash, queued, &[item], None).unwrap();
    let placeholders = crate::services::discord::runtime_store::discord_queued_placeholders_root();
    let broken = placeholders
        .unwrap()
        .join("claude")
        .join(&shared.token_hash);
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join(format!("{BROKEN}.json")), "not json").unwrap();
    let _hosts = crate::config::session_hosts::force_for_test(
        Some("mini"),
        &[(QUEUED, "mini"), (BROKEN, "mini"), (BOUND, "mini")],
    );
    let (_log, _rest) = super::super::shared_state::test_rest::recording_mock(1, QUEUED).await;

    // The production standby order: the provider reconcile settles before the homes start.
    let registry = Arc::new(health::HealthRegistry::new());
    register_standby_and_settle_reconcile(&registry, &provider, &shared).await;
    let on = settings(Some(true));
    let started = start_channel_homes(&shared, &provider, HomeRole::Standby, &on);
    assert_eq!(started.await.len(), 3);

    let mut seen = Vec::new();
    let busy = blocker_reaches(QUEUED, "turn_running", &mut seen).await;
    assert_eq!(
        seen,
        ["turn_unreadable", "turn_running"],
        "unknown until restored"
    );
    assert!(busy);
    let mailbox = shared.mailbox(queued).try_snapshot().await.unwrap();
    let restored = mailbox
        .intervention_queue
        .iter()
        .map(|item| item.message_id.get());
    assert_eq!(
        restored.collect::<Vec<_>>(),
        [ITEM],
        "the persisted queue is in the mailbox"
    );
    let mut unread = Vec::new();
    assert!(blocker_reaches(BROKEN, "turn_unreadable", &mut unread).await);

    let persistence =
        crate::services::discord::queue_persistence_context(&shared, &provider, queued);
    let mailbox = shared.mailbox(queued);
    let cleared = mailbox
        .cancel_queued_primary_message(MessageId::new(ITEM), persistence)
        .await;
    assert!(
        cleared.removed.is_some(),
        "the mailbox clears the restored item"
    );
    assert!(reclaimed(&pool, QUEUED, &mut seen).await, "{seen:?}");
    assert_eq!(
        seen,
        ["turn_unreadable", "turn_running"],
        "nothing owed is unknown"
    );
    let store = root.join("o_store").join(QUEUED.to_string());
    let store = std::fs::symlink_metadata(store).map_err(|error| error.kind());
    assert_eq!(store.err(), Some(std::io::ErrorKind::NotFound), "no store");
    let mut held = Vec::new();
    let bound = blocker_reaches(BOUND, "owed_unreadable", &mut held).await;
    assert!(bound, "a channel bound here waits: {held:?}");
    assert_eq!(
        blocker(BROKEN).as_deref(),
        Some("turn_unreadable"),
        "a failed restore stays unknown"
    );
    assert_eq!(unread, ["turn_unreadable"]);
    for channel in [BROKEN, BOUND].map(|id| id.to_string()) {
        let row = o_channel_homes::read_home(&pool, &channel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (row.state, row.holder.as_deref()),
            (o_channel_homes::HomeState::Reclaiming, Some("mini"))
        );
    }
    assert_eq!(binding_events::test_root().as_deref(), Some(root.as_path()));
    for channel in [QUEUED, BROKEN, BOUND].map(|id| id.to_string()) {
        channel_home_boot::stop(&channel).await;
        channel_home::unregister(&channel);
    }
    pool.close().await;
    db.drop().await;
}

/// A REST worker whose boot started no writer arms its own start, so a channel delegated later
/// gets a writer under its home; the precheck keeps the row released while P5 binds no source.
#[tokio::test]
async fn a_channel_delegated_after_a_rest_worker_boot_gets_a_writer_under_its_home_pg() {
    use crate::services::cluster::channel_home_boot::events::{self, Event};
    const LATER: u64 = 9_300_000_000_000_016;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = LATER.to_string();
    let shared = super::super::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    shared.restart.reconcile_done.store(true, Ordering::SeqCst);
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(LATER, "mini")]);
    let _selected = test_override::force_candidates(&[(LATER, ClaudeTui)]);
    let (_log, _rest) = super::super::shared_state::test_rest::recording_mock(1, LATER).await;
    events::clear(LATER);
    let settings_on = settings(Some(true));
    let started = start_channel_homes(
        &shared,
        &ProviderKind::Claude,
        HomeRole::RestWorker,
        &settings_on,
    );
    assert!(started.await.is_empty(), "no row names this node at boot");
    let writers = o_writer_host::spawn_delegated(&shared, &ProviderKind::Claude, None);
    assert!(writers.is_empty(), "nothing is delegated here yet");

    let applied = |write: Result<HomeWrite<o_channel_homes::ChannelHome>, _>| match write {
        Ok(HomeWrite::Applied(row)) => row,
        other => panic!("{other:?}"),
    };
    let row = applied(o_channel_homes::delegate(&pool, &channel, "claude", "gw", "mini").await);
    applied(o_channel_homes::finish_release(&pool, &channel, "gw", row.epoch).await);
    let mut home = None;
    for _ in 0..1_000 {
        home = channel_home::registered(&channel);
        let gate = home.as_ref().map(|home| Arc::as_ptr(&home.gate()) as usize);
        let under = |event: &Event| matches!(event, Event::Started(_, at) if Some(*at) == gate);
        if events::of(LATER).iter().any(under) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let home = home.expect("the delegation mounted a home");
    let gate = Arc::as_ptr(&home.gate()) as usize;
    let under = |event: &Event| matches!(event, Event::Started(_, at) if *at == gate);
    assert!(
        events::of(LATER).iter().any(under),
        "{:?}",
        events::of(LATER)
    );
    let readiness = crate::services::tui_o::writer::host::process_readiness();
    assert!(readiness.hosted_by(LATER).is_some());
    let refused = Event::Adoptable(Err("adopt_refused:no_bound_source"));
    for _ in 0..1_000 {
        if events::of(LATER).contains(&refused) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        events::of(LATER).contains(&refused),
        "{:?}",
        events::of(LATER)
    );
    let row = o_channel_homes::read_home(&pool, &channel)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, o_channel_homes::HomeState::Released);
    use crate::services::cluster::home_supervisor::{HomeLifecycle, StopReason};
    let homes = o_writer_host::home_writers(&ProviderKind::Claude, true);
    homes.stop_and_join(StopReason::Sigterm).await;
    channel_home::unregister(&channel);
    events::clear(LATER);
    pool.close().await;
    db.drop().await;
}

/// Binds each channel to an agent in this runtime's config, so a standby restore owns it.
fn bind_agents(root: &std::path::Path, channels: &[u64]) {
    let agent = |id: &u64| {
        let channel = format!("    channels:\n      claude:\n        id: \"{id}\"\n");
        format!("  - id: sbr-{id}\n    name: \"sbr {id}\"\n    provider: claude\n{channel}")
    };
    let agents: String = channels.iter().map(agent).collect();
    let config = crate::runtime_layout::config_file_path(root);
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, format!("server:\n  port: 8791\nagents:\n{agents}")).unwrap();
}

/// Binds an empty transcript for the channel in this thread's P5 log, as a Claude hook would.
fn bind_claude(root: &std::path::Path, channel: u64) {
    let path = root.join(format!("{channel}.jsonl"));
    std::fs::write(&path, b"").unwrap();
    let hook = binding_events::HookSignal::from_payload(
        "session_start",
        &serde_json::json!({"source": "startup"}),
    );
    let proposal = binding_events::Proposal {
        channel_id: channel,
        provider: "claude",
        tmux_session: "tmux-claude",
        session_id: Some("s1"),
        path: path.to_str().unwrap(),
        replaced: None,
        cause: binding_events::CauseSource::Hook(hook.cause()),
        hook: Some(&hook),
    };
    binding_events::record_source(&proposal).unwrap();
}

/// The message ids in the channel's mailbox queue.
async fn queued(shared: &SharedData, channel: u64) -> Vec<u64> {
    let mailbox = shared.mailbox(ChannelId::new(channel));
    let queue = mailbox.try_snapshot().await.unwrap().intervention_queue;
    queue.iter().map(|item| item.message_id.get()).collect()
}

/// The channel's home intake, if this node holds it at `epoch`.
fn intake_at(home: &channel_home::HomeGate, epoch: i64) -> Option<channel_home::HomeIntake> {
    match home.ownership() {
        HomeOwnership::Owned {
            home_epoch, intake, ..
        } if home_epoch == epoch => Some(intake),
        _ => None,
    }
}

/// A standby adopts its released home unrestored once its writer could run the first init; intake
/// stays closed while its restore runs, and the restored queue is in the mailbox before it opens.
#[tokio::test]
async fn a_standby_adopts_its_released_home_and_opens_intake_only_after_its_restore_pg() {
    use super::queued_recovery::boot_restore_tests::{item, pause};
    use crate::services::cluster::channel_home::HomeIntake;
    use crate::services::cluster::channel_home_boot::events::{self, Event};
    use crate::services::tui_o::cutover::intake_route;
    use crate::services::turn_orchestrator::save_channel_queue;
    const CH: u64 = 9_300_000_000_000_041;
    const ITEM: u64 = 9_300_000_000_000_042;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = crate::config::runtime_root().expect("runtime root");
    let _p5 = P5Root::install(&root);
    bind_agents(&root, &[CH]);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = CH.to_string();
    let applied = |write: Result<HomeWrite<o_channel_homes::ChannelHome>, _>| match write {
        Ok(HomeWrite::Applied(row)) => row,
        other => panic!("{other:?}"),
    };
    let row = applied(o_channel_homes::delegate(&pool, &channel, "claude", "gw", "mini").await);
    let released = applied(o_channel_homes::finish_release(&pool, &channel, "gw", row.epoch).await);
    let epoch = released.epoch;
    let shared = super::super::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    shared.restart.reconcile_done.store(false, Ordering::SeqCst);
    let provider = ProviderKind::Claude;
    let token = shared.token_hash.clone();
    save_channel_queue(&provider, &token, ChannelId::new(CH), &[item(ITEM)], None).unwrap();
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(CH, "mini")]);
    let _selected = test_override::force_candidates(&[(CH, ClaudeTui)]);
    let (_log, _rest) = super::super::shared_state::test_rest::recording_mock(1, CH).await;
    events::clear(CH);
    let paused = pause(CH);

    // The production standby order: reconcile settles, then the homes, then the delegated writers.
    let registry = Arc::new(health::HealthRegistry::new());
    register_standby_and_settle_reconcile(&registry, &provider, &shared).await;
    let on = settings(Some(true));
    let started = start_channel_homes(&shared, &provider, HomeRole::Standby, &on).await;
    assert_eq!(started.len(), 1, "the released row names this node");
    o_writer_host::spawn_delegated(&shared, &provider, None);
    let read = || o_channel_homes::read_home(&pool, &channel);
    let refused = Event::Adoptable(Err("adopt_refused:no_bound_source"));
    for _ in 0..1_500 {
        if events::of(CH)
            .iter()
            .filter(|event| **event == refused)
            .count()
            >= 2
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let judged = events::of(CH)
        .iter()
        .filter(|event| **event == refused)
        .count();
    assert!(judged >= 2, "{:?}", events::of(CH));
    let unbound = read().await.unwrap();
    assert_eq!(
        unbound,
        Some(released.clone()),
        "adopted with no first init"
    );

    bind_claude(&root, CH);
    let mut adopted = None;
    for _ in 0..1_500 {
        adopted = read().await.unwrap();
        if adopted.as_ref().is_some_and(|row| row.holder.is_some()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let adopted = adopted.expect("the home row");
    let held = (
        adopted.holder.as_deref(),
        adopted.target.as_deref(),
        adopted.epoch,
    );
    assert_eq!(held, (Some("mini"), None, epoch), "{:?}", events::of(CH));
    for _ in 0..1_500 {
        if paused.entered() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(paused.entered(), "the channel's restore runs once held");
    let home = &started[0];
    assert_eq!(
        intake_at(home, epoch),
        Some(HomeIntake::Closed),
        "held, closed"
    );
    assert!(channel_home::intake_hold(&channel, Some(epoch)).is_some());
    assert!(
        intake_route::held_channels("claude").contains(&channel),
        "rows wait"
    );
    assert_eq!(
        queued(&shared, CH).await,
        Vec::<u64>::new(),
        "nothing restored yet"
    );

    paused.release();
    let mut opened = None;
    for _ in 0..1_500 {
        if intake_at(home, epoch) == Some(HomeIntake::Open) {
            opened = Some(queued(&shared, CH).await);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(opened, Some(vec![ITEM]), "opened with the queue restored");
    assert_eq!(channel_home::intake_hold(&channel, Some(epoch)), None);
    assert_eq!(channel_home_boot::running(&channel), (true, Some(epoch)));
    use crate::services::cluster::home_supervisor::{HomeLifecycle, StopReason};
    let homes = o_writer_host::home_writers(&ProviderKind::Claude, true);
    homes.stop_and_join(StopReason::Sigterm).await;
    channel_home::unregister(&channel);
    events::clear(CH);
    pool.close().await;
    db.drop().await;
}

/// A standby holder's failed restore keeps intake closed from its first renewal on, the persisted
/// queue untouched; a restart after the cause is gone restores it and opens.
#[tokio::test]
async fn a_standby_holder_whose_restore_fails_keeps_intake_closed_until_a_restart_restores_pg() {
    use super::queued_recovery::boot_restore_tests::item;
    use crate::services::cluster::channel_home::HomeIntake;
    use crate::services::tui_o::cutover::intake_route;
    use crate::services::turn_orchestrator::save_channel_queue;
    const CH: u64 = 9_300_000_000_000_043;
    const ITEM: u64 = 9_300_000_000_000_044;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = crate::config::runtime_root().expect("runtime root");
    bind_agents(&root, &[CH]);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = CH.to_string();
    let applied = |write: Result<HomeWrite<o_channel_homes::ChannelHome>, _>| match write {
        Ok(HomeWrite::Applied(row)) => row,
        other => panic!("{other:?}"),
    };
    let row = applied(o_channel_homes::delegate(&pool, &channel, "claude", "gw", "mini").await);
    let row = applied(o_channel_homes::finish_release(&pool, &channel, "gw", row.epoch).await);
    let epoch = applied(o_channel_homes::adopt(&pool, &channel, "mini", row.epoch).await).epoch;
    let shared = super::super::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    shared.restart.reconcile_done.store(false, Ordering::SeqCst);
    let provider = ProviderKind::Claude;
    let token = shared.token_hash.clone();
    save_channel_queue(&provider, &token, ChannelId::new(CH), &[item(ITEM)], None).unwrap();
    // An unreadable placeholder store fails the channel's restore.
    let placeholders = crate::services::discord::runtime_store::discord_queued_placeholders_root();
    let broken = placeholders.unwrap().join("claude").join(&token);
    std::fs::create_dir_all(&broken).unwrap();
    let broken = broken.join(format!("{CH}.json"));
    std::fs::write(&broken, "not json").unwrap();
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(CH, "mini")]);
    let (_log, _rest) = super::super::shared_state::test_rest::recording_mock(1, CH).await;
    let registry = Arc::new(health::HealthRegistry::new());
    register_standby_and_settle_reconcile(&registry, &provider, &shared).await;
    let on = settings(Some(true));
    let started = start_channel_homes(&shared, &provider, HomeRole::Standby, &on).await;
    assert_eq!(started.len(), 1);
    o_writer_host::spawn_delegated(&shared, &provider, None);
    let mut held = false;
    for _ in 0..1_000 {
        held = intake_at(&started[0], epoch).is_some();
        if held {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(held, "the lease holds the home");
    // Two more rounds: the renewal then the restore, which fails, have both run.
    let judged = channel_home_boot::rounds(CH) + 2;
    for _ in 0..1_000 {
        if channel_home_boot::rounds(CH) >= judged {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(channel_home_boot::rounds(CH) >= judged);
    let closed = intake_at(&started[0], epoch);
    assert_eq!(
        closed,
        Some(HomeIntake::Closed),
        "a failed restore keeps intake closed"
    );
    assert!(
        intake_route::held_channels("claude").contains(&channel),
        "rows wait"
    );
    use crate::services::turn_orchestrator::load_channel_pending_queue_checked as load;
    let kept = load(&provider, &token, ChannelId::new(CH));
    let kept = kept
        .unwrap()
        .0
        .iter()
        .map(|item| item.message_id.get())
        .collect::<Vec<_>>();
    assert_eq!(kept, [ITEM], "the persisted queue is kept");
    assert_eq!(queued(&shared, CH).await, Vec::<u64>::new());

    std::fs::remove_file(&broken).unwrap();
    channel_home_boot::stop(&channel).await;
    channel_home::unregister(&channel);
    let restarted = start_channel_homes(&shared, &provider, HomeRole::Standby, &on).await;
    assert_eq!(restarted.len(), 1);
    let mut opened = None;
    for _ in 0..1_500 {
        if intake_at(&restarted[0], epoch) == Some(HomeIntake::Open) {
            opened = Some(queued(&shared, CH).await);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        opened,
        Some(vec![ITEM]),
        "restarted, it restores the queue and opens"
    );
    use crate::services::cluster::home_supervisor::{HomeLifecycle, StopReason};
    let homes = o_writer_host::home_writers(&ProviderKind::Claude, true);
    homes.stop_and_join(StopReason::Sigterm).await;
    channel_home::unregister(&channel);
    pool.close().await;
    db.drop().await;
}
