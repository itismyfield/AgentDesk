use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::inflight::o_seed_observation::Guard;
use crate::services::tui_prompt_dedupe::{TuiRuntimeBinding, register_tmux_runtime_binding};
use crate::services::tmux_common::{session_temp_path, write_tmux_runtime_kind_marker};

fn fixture_pane(state: &str) {
    let root = crate::config::runtime_root().unwrap();
    std::fs::write(root.join("pane.next"), format!("{state}\n")).unwrap();
    std::fs::rename(root.join("pane.next"), root.join("pane")).unwrap();
}

fn native_host_proof(fixture: &crate::services::discord::tmux::InstallFixture) {
    std::fs::write(session_temp_path(&fixture.session, "host_kind"), "tmux").unwrap();
    register_tmux_runtime_binding(&fixture.session, TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: fixture.output.clone(), relay_output_path: None, input_fifo_path: None,
        session_id: Some("real-installer-host-source".into()), last_offset: 0, relay_last_offset: None,
    });
    assert!(crate::services::tui_prompt_dedupe::try_peek_tmux_runtime_binding(&fixture.session).unwrap().is_some());
}

async fn require_kept_outer_and_inner(
    fixture: &crate::services::discord::tmux::InstallFixture,
    observed: &Guard,
) {
    use super::o_seed_install_tests::{append, until};
    let before = observed.snapshot();
    fixture_pane("dead");
    std::fs::write(crate::services::tmux_common::session_dead_marker_path(&fixture.session), "dead").unwrap();
    until(observed, "kept outer EOF after host proof changed", |s| {
        s.event_count("outer_eof") >= before.event_count("outer_eof") + 2
            && s.event_count("host_probe") >= before.event_count("host_probe") + 2
    }).await;
    let outer = observed.snapshot();
    let fresh = said("host-fenced fresh nonterminal body");
    append(fixture, fresh.as_bytes());
    until(observed, "real append and kept inner EOF after host proof changed", |s| {
        s.parser_responses.iter().any(|r| r == "host-fenced fresh nonterminal body")
            && s.event_count("inner_eof") >= outer.event_count("inner_eof") + 2
            && s.event_count("host_probe") >= outer.event_count("host_probe") + 2
    }).await;
    assert_eq!(observed.snapshot().event_count("rowless_host_probe"), outer.event_count("rowless_host_probe"), "settled unproved hosts are not probed as tmux in the inner collector");
}

async fn fixture_on_pg(pool: &sqlx::PgPool, channel: u64) -> (crate::services::discord::tmux::InstallFixture, crate::services::discord::health::legacy_supervision::test_support::MockDiscord) {
    let (mut fixture, discord) = super::o_seed_install_tests::fixture(channel).await;
    let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    Arc::get_mut(&mut shared).unwrap().health_registry = Arc::downgrade(&fixture.registry);
    {
        let mut settings = shared.settings.write().await;
        settings.provider = CLAUDE;
        settings.allowed_channel_ids = vec![channel];
    }
    fixture.registry.register("claude".into(), shared.clone()).await;
    fixture.shared = shared;
    seed_host_record(pool, &fixture, None).await;
    (fixture, discord)
}

async fn seed_host_record(pool: &sqlx::PgPool, fixture: &crate::services::discord::tmux::InstallFixture, record: Option<serde_json::Value>) {
    let key = crate::services::discord::host_teardown_gate::test_support::channel_key(&fixture.shared, &fixture.session);
    crate::services::discord::inflight::seed_session_row_hashed(pool, &key, fixture.channel.get(), &fixture.shared.token_hash, record).await;
}

fn hosted_record(fixture: &crate::services::discord::tmux::InstallFixture, unknown: bool) -> serde_json::Value {
    use crate::db::dispatched_sessions::hosted_execution::tests::{future_schema, owner, record, wire};
    use crate::db::dispatched_sessions::hosted_execution::HostedState;
    let mut owner = owner(&fixture.channel.get().to_string());
    owner.discord_token_hash = fixture.shared.token_hash.clone();
    owner.logical_key = fixture.session.clone();
    if unknown { future_schema(&owner) } else { wire(&record(&owner, "host-proof-changed", HostedState::Bound)) }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t6_f_installer_pg_locator_changes_before_and_after_actual_host_await_keep_both_eofs() {
    use super::o_seed_install_tests::{assert_rowless, observe, seed, until};
    use crate::services::discord::health::legacy_supervision::{RetiredForTest, test_support::tree_fingerprint};
    use crate::services::discord::tmux::{OOnlyInstallOutcome, watch_host_of};
    let test = "t6_f_installer_pg_locator_changes_before_and_after_actual_host_await_keep_both_eofs";
    if !isolated_in("retired_host_tick_tests", test, &[]) { return; }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (index, change) in ["before-herdr-locator", "before-unknown-record", "after-await-herdr-locator"].into_iter().enumerate() {
        let (fixture, _discord) = fixture_on_pg(&pool, 632_511_610 + index as u64).await;
        native_host_proof(&fixture);
        seed(&fixture, 0);
        assert_eq!(watch_host_of(&fixture.shared, &CLAUDE, fixture.channel.get(), &fixture.session).await, WatchHost::Legacy);
        let _retired = RetiredForTest::new("claude", fixture.channel.get());
        let row_root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
        let fingerprint = tree_fingerprint(&row_root);
        let observed = observe(&fixture);
        assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
        until(&observed, "actual PG-backed initial EOF on a proven tmux host", |s| s.event_count("outer_eof") >= 2 && s.event_count("rowless_host_probe") >= 2).await;
        let paused = if change == "after-await-herdr-locator" {
            let (entered, release) = host_gate::retired_probe_test::pause_after_probe(fixture.channel.get());
            fixture_pane("dead");
            std::fs::write(crate::services::tmux_common::session_dead_marker_path(&fixture.session), "dead").unwrap();
            tokio::time::timeout(Duration::from_secs(10), entered).await.unwrap().unwrap();
            Some(release)
        } else { None };
        let raw = hosted_record(&fixture, change == "before-unknown-record");
        if change != "before-unknown-record" {
            assert_eq!(raw["location"]["host"], "herdr", "fixture really records a Herdr locator beside a tmux marker");
        }
        seed_host_record(&pool, &fixture, Some(raw)).await;
        assert_eq!(watch_host_of(&fixture.shared, &CLAUDE, fixture.channel.get(), &fixture.session).await, if change == "before-unknown-record" { WatchHost::Unverified } else { WatchHost::Herdr }, "actual sessions-row host delta: {change}");
        if let Some(release) = paused { release.send(()).unwrap(); }
        require_kept_outer_and_inner(&fixture, &observed).await;
        fixture.cancel_and_join().await;
        assert_rowless(&observed);
        assert_eq!(tree_fingerprint(&row_root), fingerprint, "{change}");
        eprintln!("T6F_PG_LOCATOR={change} key={:?} raw={:?}", observed.snapshot(), observed.raw_process_snapshot());
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t6_f_installer_keeps_changed_herdr_unknown_runtime_and_missing_marker_through_both_eofs() {
    use super::o_seed_install_tests::{assert_rowless, fixture, observe, seed, until};
    use crate::services::discord::health::legacy_supervision::{RetiredForTest, test_support::tree_fingerprint};
    use crate::services::discord::tmux::OOnlyInstallOutcome;
    let test = "t6_f_installer_keeps_changed_herdr_unknown_runtime_and_missing_marker_through_both_eofs";
    if !isolated_in("retired_host_tick_tests", test, &[]) { return; }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for (index, change) in ["herdr", "unknown-runtime", "missing-marker"].into_iter().enumerate() {
        let (fixture, _discord) = fixture(632_511_600 + index as u64).await;
        native_host_proof(&fixture);
        seed(&fixture, 0);
        let _retired = RetiredForTest::new("claude", fixture.channel.get());
        let row_root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
        let fingerprint = tree_fingerprint(&row_root);
        let observed = observe(&fixture);
        assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
        until(&observed, "valid install completed real initial tmux EOF probe", |s| {
            s.event_count("outer_eof") >= 2 && s.event_count("rowless_host_probe") >= 2
        }).await;
        match change {
            "herdr" => std::fs::write(session_temp_path(&fixture.session, "host_kind"), "herdr").unwrap(),
            "unknown-runtime" => std::fs::write(session_temp_path(&fixture.session, crate::services::tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT), "future-runtime").unwrap(),
            "missing-marker" => std::fs::remove_file(session_temp_path(&fixture.session, "host_kind")).unwrap(),
            _ => unreachable!(),
        }
        require_kept_outer_and_inner(&fixture, &observed).await;
        fixture.cancel_and_join().await;
        assert_rowless(&observed);
        assert_eq!(tree_fingerprint(&row_root), fingerprint, "{change}");
        eprintln!("T6F_CHANGED_PROOF={change} key={:?} raw={:?}", observed.snapshot(), observed.raw_process_snapshot());
    }
}

fn binding(h: &Harness) -> TuiRuntimeBinding {
    TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: h.path.clone(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some("host-proof-session".into()),
        last_offset: 0,
        relay_last_offset: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_host_probe_requires_independent_tmux_and_native_source_evidence() {
    let test = "retired_host_probe_requires_independent_tmux_and_native_source_evidence";
    if !isolated_in("retired_host_tick_tests", test, &[]) {
        return;
    }
    let h = Harness::new(63_251, "").await;
    h.pane("dead");
    for case in ["missing-marker", "herdr-marker", "unknown-marker", "snapshot-herdr", "snapshot-unverified", "unknown-runtime", "runtime-mismatch", "source-mismatch", "missing-session", "absent-binding", "missing-source", "busy-binding"] {
        let mut candidate = binding(&h);
        let mut snapshot = WatchHost::Legacy;
        let marker = session_temp_path(&h.tmux, "host_kind");
        std::fs::write(&marker, "tmux").unwrap();
        write_tmux_runtime_kind_marker(&h.tmux, RuntimeHandoffKind::ClaudeTui).unwrap();
        match case {
            "missing-marker" => std::fs::remove_file(&marker).unwrap(),
            "herdr-marker" => std::fs::write(&marker, "herdr").unwrap(),
            "unknown-marker" => std::fs::write(&marker, "future-host").unwrap(),
            "snapshot-herdr" => snapshot = WatchHost::Herdr,
            "snapshot-unverified" => snapshot = WatchHost::Unverified,
            "unknown-runtime" => std::fs::write(session_temp_path(&h.tmux, crate::services::tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT), "future-runtime").unwrap(),
            "runtime-mismatch" => write_tmux_runtime_kind_marker(&h.tmux, RuntimeHandoffKind::CodexTui).unwrap(),
            "source-mismatch" => candidate.output_path.push_str(".other"),
            "missing-session" => candidate.session_id = None,
            "missing-source" => std::fs::remove_file(&h.path).unwrap(),
            _ => {}
        }
        register_tmux_runtime_binding(&h.tmux, candidate);
        if case == "absent-binding" {
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&h.tmux);
        }
        let busy = (case == "busy-binding").then(crate::services::tui_prompt_dedupe::hold_binding_peek_lock_for_tests);
        h.take_tmux_calls();
        let observation = Guard::new(&CLAUDE, h.channel.get());
        assert!(host_gate::tmux_alive_for_mode(&h.shared, &h.tmux, h.channel, &HostSnapshot::new(snapshot), WatcherLegacyMode::RetiredO, &h.path).await, "{case}");
        let seen = observation.raw_process_snapshot();
        assert_eq!(seen, observation.snapshot(), "raw observations are not filtered: {case}");
        assert_eq!(seen.event_count("host_probe"), 1, "actual host gate reached: {case}");
        assert_eq!(seen.event_count("rowless_host_probe"), 0, "unproved host never probed as tmux: {case}");
        assert_eq!((seen.writable_inflight_load_calls, seen.readonly_inflight_load_calls), (0, 0), "{case}: {seen:?}");
        assert!(h.take_tmux_calls().is_empty(), "{case}");
        drop(observation);
        drop(busy);
        if case == "missing-source" {
            std::fs::write(&h.path, "").unwrap();
        }
    }
    println!("strict retired host proof matrix completed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_host_probe_keeps_unanswered_and_preserves_legacy_liveness() {
    let test = "retired_host_probe_keeps_unanswered_and_preserves_legacy_liveness";
    if !isolated_in("retired_host_tick_tests", test, &[]) {
        return;
    }
    let h = Harness::new(63_252, "").await;
    std::fs::write(session_temp_path(&h.tmux, "host_kind"), "tmux").unwrap();
    write_tmux_runtime_kind_marker(&h.tmux, RuntimeHandoffKind::ClaudeTui).unwrap();
    register_tmux_runtime_binding(&h.tmux, binding(&h));
    for (pane, alive) in [("dead", false), ("unanswered", true), ("busy", true)] {
        h.pane(pane);
        let marker = crate::services::tmux_common::session_dead_marker_path(&h.tmux);
        let before = std::fs::read(&marker).unwrap();
        h.take_tmux_calls();
        let observation = Guard::new(&CLAUDE, h.channel.get());
        assert_eq!(host_gate::tmux_alive_for_mode(&h.shared, &h.tmux, h.channel, &HostSnapshot::new(WatchHost::Legacy), WatcherLegacyMode::RetiredO, &h.path).await, alive, "{pane}");
        let seen = observation.raw_process_snapshot();
        assert_eq!(seen, observation.snapshot());
        assert_eq!(seen.event_count("rowless_host_probe"), 1, "actual host probe ran: {pane}");
        assert_eq!((seen.writable_inflight_load_calls, seen.readonly_inflight_load_calls), (0, 0), "{seen:?}");
        assert!(h.take_tmux_calls().iter().any(|call| call.starts_with("has-session")));
        assert_eq!(std::fs::read(&marker).unwrap(), before, "O probe never gains marker cleanup authority");
        drop(observation);
    }
    h.pane("dead");
    let observation = Guard::new(&CLAUDE, h.channel.get());
    assert!(!host_gate::tmux_alive_for_mode(&h.shared, &h.tmux, h.channel, &HostSnapshot::new(WatchHost::Legacy), WatcherLegacyMode::Legacy, &h.path).await);
    let seen = observation.raw_process_snapshot();
    assert_eq!(seen.readonly_inflight_load_calls, 1, "Legacy keeps its existing row probe: {seen:?}");
    println!("Legacy host raw observations: {seen:?}");
}
