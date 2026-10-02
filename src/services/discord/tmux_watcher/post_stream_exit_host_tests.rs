//! The real watcher loop on Legacy and O channels: only a session the keyed host gate admits
//! is killed, and only a local tmux death ends a turn or clears its row.

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::host_teardown_gate::test_support::{Stored, channel_key, seed};
use crate::services::tmux_common::{session_dead_marker_path, session_temp_path};

const T0: &str = "ADK-P4B2 T0 delivered before the watcher attached";
const STREAMING: &str = "ADK-P4B2 T1 streaming when the pane dies";
const RESET: &str = "세션을 초기화했습니다";
const NOT_RESET: &str = "세션을 초기화하지 않았습니다";
const LEGACY_BASE: u64 = 40;
const O_BASE: u64 = 60;
const PER_GROUP: u64 = 20;

fn turn(prompt: &str, body: &str) -> String {
    format!("{}{}{}", user(prompt), said(body), stop())
}

fn o_channels() -> String {
    let ids: Vec<String> = (O_BASE..O_BASE + PER_GROUP)
        .map(|case| format!("[{},\"claude_tui\"]", 6_284_100 + case))
        .collect();
    format!("[{}]", ids.join(","))
}

/// The host evidence the `.host_kind` marker and the inflight row carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Host {
    /// No marker and no row locator.
    Local,
    /// A Herdr marker.
    Marker,
    /// A row bound to a Herdr pane.
    Locator,
    /// A row locator this build cannot read.
    UnknownLocator,
    /// A tmux marker beside a row bound to a Herdr pane.
    Conflict,
    /// A row bound to a Herdr pane that names no tmux session.
    Nameless,
}

/// A watcher attached past a delivered T0 with its row, on an O-bound session when `o`.
/// `stored` is the sessions row when `pool` is given; `exit` is the recorded exit reason.
async fn attached(
    case: u64,
    o: bool,
    host: Host,
    db: Option<(&sqlx::PgPool, Stored)>,
    exit: Option<&str>,
) -> (
    Harness,
    Option<crate::services::tui_o::cutover::test_override::TuiSessionGuard>,
) {
    let seed_text = turn("T0", T0);
    let mut h = Harness::on(case, &seed_text, db.map(|(pool, _)| pool.clone())).await;
    if let Some((pool, stored)) = db {
        let key = channel_key(&h.shared, &h.tmux);
        seed(pool, &key, &h.tmux, h.channel.get(), stored).await;
    }
    let f = seed_text.len() as u64;
    h.commit(0, f);
    let bound = o.then(|| {
        crate::services::tui_o::cutover::test_override::bind_claude_tui_session(&h.tmux, &h.path)
    });
    let marker = match host {
        Host::Marker => Some("herdr"),
        Host::Conflict => Some("tmux"),
        _ => None,
    };
    if let Some(marker) = marker {
        std::fs::write(session_temp_path(&h.tmux, "host_kind"), marker).unwrap();
    }
    if let Some(exit) = exit {
        crate::services::tmux_diagnostics::record_tmux_exit_reason(&h.tmux, exit);
    }
    let mut row = serde_json::to_value(h.row_at(f)).unwrap();
    let herdr = serde_json::json!({"host_kind": "herdr", "host_session_id": "w1", "pane": "w1-1"});
    match host {
        Host::Locator | Host::Conflict | Host::Nameless => row["host_locator"] = herdr,
        Host::UnknownLocator => {
            row["host_locator"] =
                serde_json::json!({"host_kind": "zellij", "host_session_id": "z1"})
        }
        Host::Local | Host::Marker => {}
    }
    if host == Host::Nameless {
        row["tmux_session_name"] = serde_json::Value::Null;
    }
    if row["host_locator"].is_object() {
        row["hosted_record_id"] = serde_json::json!(7);
        row["hosted_execution_nonce"] = serde_json::json!("p4b2-nonce");
        h.save(&serde_json::from_value(row).unwrap());
        assert!(
            h.row().unwrap().host_locator.is_some(),
            "the binding is stored"
        );
    }
    h.spawn(f);
    (h, bound)
}

/// Removal reasons this channel's inflight row logged.
fn removals(h: &Harness) -> Vec<String> {
    let removed = h.events("inflight state row removal");
    removed.iter().filter_map(|l| field(l, "reason")).collect()
}

#[derive(Clone, Copy, Debug)]
enum End {
    Death,
    Cancel,
}

// The post-stream clear and kill run only past the keyed host gate (sessions row, marker and
// inflight row) and only on a pane tmux or the wrapper confirms dead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_exit_tears_down_only_a_confirmed_local_tmux_death_pg() {
    let channels = o_channels();
    let o = crate::services::tui_o::cutover::test_override::CHANNELS_ENV;
    let flag = crate::services::tui_o::cutover::test_override::CHILD_ENV;
    if !isolated_in(
        "post_stream_exit_host_tests",
        "watcher_exit_tears_down_only_a_confirmed_local_tmux_death_pg",
        &[(flag, "1"), (o, &channels)],
    ) {
        return;
    }
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    use End::{Cancel, Death};
    use Stored::{Future, Hosted, Legacy, Missing};
    // (sessions row, local host, pane, `.pane_dead`, how the watcher ends, row kept, kills)
    let cases = [
        (Missing, Host::Local, "deadpane", false, Death, false, 1),
        (Legacy, Host::Local, "deadpane", false, Death, false, 1),
        (Hosted, Host::Local, "deadpane", false, Cancel, true, 0),
        (Future, Host::Local, "deadpane", false, Death, true, 0),
        (Missing, Host::Local, "unanswered", false, Cancel, true, 0),
        (Missing, Host::Local, "unanswered", true, Death, false, 0),
        (Missing, Host::Local, "listfail", false, Cancel, true, 0),
        (Missing, Host::Marker, "deadpane", true, Cancel, true, 0),
        (Missing, Host::Locator, "deadpane", false, Cancel, true, 0),
        (Missing, Host::Nameless, "deadpane", false, Cancel, true, 0),
    ];
    for (base, o) in [(LEGACY_BASE, false), (O_BASE, true)] {
        for (n, (stored, host, pane, pane_dead, end, row_kept, kills)) in
            cases.into_iter().enumerate()
        {
            let db = Some((&pool, stored));
            let done = Some("turn completed");
            let (mut h, bound) = attached(base + n as u64, o, host, db, done).await;
            // The pane changes first: a `.pane_dead` beside a live pane is cleared as stale.
            h.pane(pane);
            if pane_dead {
                std::fs::write(session_dead_marker_path(&h.tmux), "dead").unwrap();
            }
            if let Cancel = end {
                h.cancel();
            }
            let label = format!("o={o} {stored:?} {host:?} {pane} pane_dead={pane_dead}");
            h.exited(&label).await;
            assert_eq!((h.row().is_some(), h.kills()), (row_kept, kills), "{label}");
            let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session;
            assert_eq!(binding(&h.tmux).is_some(), bound.is_some(), "{label}");
        }
    }
    pool.close().await;
    db.drop().await;
}

// A streaming turn whose pane dies with no exit reason hands off and clears its row only when
// the row records no other host; otherwise the watcher keeps reading and the row stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collector_death_keeps_a_turn_bound_to_another_host() {
    if !isolated_in(
        "post_stream_exit_host_tests",
        "collector_death_keeps_a_turn_bound_to_another_host",
        &[],
    ) {
        return;
    }
    let hosts = [
        Host::Local,
        Host::Locator,
        Host::UnknownLocator,
        Host::Conflict,
        Host::Nameless,
    ];
    for (n, host) in hosts.into_iter().enumerate() {
        let (mut h, _) = attached(LEGACY_BASE + n as u64, false, host, None, None).await;
        h.append(format!("{}{}", user("T1"), said(STREAMING)).as_bytes());
        h.until("streaming preview", |h| h.showing(STREAMING)).await;
        let deferred = || Harness::logged("tmux probe deferred").len();
        let before = deferred();
        h.pane("dead");
        if host == Host::Local {
            h.exited("watcher exit").await;
            assert_eq!(removals(&h), ["clear_inflight_state"], "{host:?}");
            assert!(h.row().is_none(), "{host:?}");
            continue;
        }
        // The probe ran and deferred, and the watcher went on polling.
        h.until("deferred probe", |_| deferred() > before).await;
        let seen = crate::services::discord::tmux_watcher_now_ms();
        h.until("still polling", |h| h.heartbeat() > seen).await;
        assert!(!h.watcher_finished(), "{host:?}");
        assert!(removals(&h).is_empty(), "{host:?}");
        h.cancel();
        h.exited("watcher exit").await;
        let row = h.row().unwrap_or_else(|| panic!("{host:?}: row kept"));
        assert!(row.host_locator.is_some(), "{host:?}");
        assert!(removals(&h).is_empty(), "{host:?}");
    }
}

// After a finished turn the watcher stays attached to a session it cannot confirm dead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_commit_keeps_the_watcher_on_a_session_not_confirmed_dead() {
    let channels = o_channels();
    let o = crate::services::tui_o::cutover::test_override::CHANNELS_ENV;
    let flag = crate::services::tui_o::cutover::test_override::CHILD_ENV;
    if !isolated_in(
        "post_stream_exit_host_tests",
        "terminal_commit_keeps_the_watcher_on_a_session_not_confirmed_dead",
        &[(flag, "1"), (o, &channels)],
    ) {
        return;
    }
    let cases = [(Host::Local, "unanswered"), (Host::Marker, "deadpane")];
    for (base, o) in [(LEGACY_BASE, false), (O_BASE, true)] {
        for (n, (host, pane)) in cases.into_iter().enumerate() {
            let label = format!("o={o} {host:?} {pane}");
            let (mut h, _bound) = attached(base + n as u64, o, host, None, None).await;
            h.pane(pane);
            h.append(turn("T1", "ADK-P4B2 T1 body").as_bytes());
            h.drained("terminal frame").await;
            let seen = crate::services::discord::tmux_watcher_now_ms();
            h.until("still polling", |h| h.heartbeat() > seen).await;
            assert!(!h.watcher_finished(), "{label}");
            h.cancel();
            h.exited("watcher exit").await;
            assert_eq!(h.kills(), 0, "{label}");
        }
    }
}

// The prompt-too-long and stale-resume kills run only past the keyed host gate; a refused
// prompt-too-long kill says the session was not reset and leaves the restart handoff armed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_kills_take_the_keyed_host_verdict_pg() {
    if !isolated_in(
        "post_stream_exit_host_tests",
        "abort_kills_take_the_keyed_host_verdict_pg",
        &[],
    ) {
        return;
    }
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let result = |text: &str| {
        let line = serde_json::json!({"type": "result", "subtype": "error_during_execution",
            "is_error": true, "result": text});
        format!("{}{line}\n", user("T1"))
    };
    let prompt_too_long = result("Prompt is too long");
    let stale = result("No conversation found with session ID: adk-p4b2");
    use Stored::{Future, Hosted, Legacy, Missing};
    let stores = [
        (Missing, Host::Local),
        (Legacy, Host::Local),
        (Hosted, Host::Local),
        (Future, Host::Local),
        (Missing, Host::Marker),
    ];
    let mut case = LEGACY_BASE;
    for line in [&prompt_too_long, &stale] {
        for (stored, host) in stores {
            let admitted = matches!(stored, Missing | Legacy) && host == Host::Local;
            let label = format!("{stored:?} {host:?} {line}");
            let (mut h, _) = attached(case, false, host, Some((&pool, stored)), None).await;
            case += 1;
            if line == &stale {
                // The next turn's frame is read only after the abort line was handled.
                h.append(format!("{line}{}", turn("T2", "ADK-P4B2 T2 body")).as_bytes());
                h.drained("next turn frame").await;
                assert_eq!(h.kills(), admitted as usize, "{label}");
                continue;
            }
            h.append(line.as_bytes());
            let notice = if admitted { RESET } else { NOT_RESET };
            h.until(&label, |h| h.showing(notice)).await;
            assert_eq!(h.kills(), admitted as usize, "{label}");
            assert!(
                !h.showing(if admitted { NOT_RESET } else { RESET }),
                "{label}"
            );
            // A Herdr row takes no tmux death; elsewhere an abnormal death hands the
            // turn off only when no kill ran.
            if host != Host::Local || stored == Hosted {
                continue;
            }
            h.pane("dead");
            h.exited(&label).await;
            let handoff = removals(&h).contains(&"clear_inflight_state".to_owned());
            assert_eq!(handoff, !admitted, "{label}: {:?}", removals(&h));
        }
    }
    pool.close().await;
    db.drop().await;
}

fn set_mode(path: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Waits for `n` more "watcher kept" verdicts, so the exit the caller set up ran at least once.
async fn kept_again(h: &Harness, what: &str, n: usize) {
    let kept = || Harness::logged("watcher kept the session").len();
    let before = kept();
    h.until(what, |_| kept() >= before + n).await;
}

// A session only its sessions row places on Herdr is never probed, captured or ended as tmux at
// any exit; at a death an unverified row is read again and keeps the session only if it names Herdr.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_herdr_sessions_row_keeps_every_watcher_exit_off_tmux_pg() {
    if !isolated_in(
        "post_stream_exit_host_tests",
        "a_herdr_sessions_row_keeps_every_watcher_exit_off_tmux_pg",
        &[],
    ) {
        return;
    }
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let herdr = Some((&pool, Stored::Hosted));
    let (mut h, _) = attached(LEGACY_BASE, false, Host::Local, herdr, None).await;
    h.pane("dead");
    h.take_tmux_calls();
    kept_again(&h, "end of output", 1).await;
    set_mode(&h.path, 0o000);
    kept_again(&h, "unreadable output", 2).await;
    set_mode(&h.path, 0o644);
    let paused = h
        .shared
        .tmux_watchers
        .get(&h.channel)
        .unwrap()
        .paused
        .clone();
    paused.store(true, Ordering::Release);
    kept_again(&h, "paused", 2).await;
    paused.store(false, Ordering::Release);
    h.append(format!("{}{}", user("T1"), said(STREAMING)).as_bytes());
    h.until("streaming preview", |h| h.showing(STREAMING)).await;
    kept_again(&h, "streaming turn", 1).await;
    h.append(stop().as_bytes());
    h.drained("terminal frame").await;
    let seen = crate::services::discord::tmux_watcher_now_ms();
    h.until("still polling", |h| h.heartbeat() > seen).await;
    assert!(!h.watcher_finished(), "no exit ended the watcher");
    h.cancel();
    h.exited("watcher exit").await;
    assert_eq!(h.take_tmux_calls(), Vec::<String>::new());
    assert_eq!(h.kills(), 0);

    for (n, now, death) in [(1, Stored::Hosted, false), (2, Stored::Future, true)] {
        let unread = Some((&pool, Stored::Future));
        let (mut h, _) = attached(LEGACY_BASE + n, false, Host::Local, unread, None).await;
        let seen = crate::services::discord::tmux_watcher_now_ms();
        h.until("start read done", |h| h.heartbeat() > seen).await;
        let key = channel_key(&h.shared, &h.tmux);
        seed(&pool, &key, &h.tmux, h.channel.get(), now).await;
        h.pane("dead");
        if death {
            h.exited("death on an unreadable row").await;
            continue;
        }
        let reread = || Harness::logged("its sessions row names Herdr").len();
        h.until("row read again", |_| reread() > 0).await;
        let seen = crate::services::discord::tmux_watcher_now_ms();
        h.until("still polling", |h| h.heartbeat() > seen).await;
        assert!(!h.watcher_finished(), "the re-read kept the session");
        h.cancel();
        h.exited("watcher exit").await;
        assert_eq!(h.kills(), 0);
    }
    pool.close().await;
    db.drop().await;
}

// A Legacy watcher re-reads the marker a later launch wrote before the streaming tick captures:
// no capture, and no row re-acquired from a pane it cannot see.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_streaming_tick_rechecks_the_host_before_capturing_the_pane() {
    if !isolated_in(
        "post_stream_exit_host_tests",
        "a_streaming_tick_rechecks_the_host_before_capturing_the_pane",
        &[],
    ) {
        return;
    }
    let seed_text = turn("T0", T0);
    let mut h = Harness::new(LEGACY_BASE, &seed_text).await;
    let f = seed_text.len() as u64;
    h.commit(0, f);
    h.spawn(f);
    let seen = crate::services::discord::tmux_watcher_now_ms();
    h.until("started on Legacy", |h| h.heartbeat() > seen).await;
    std::fs::write(session_temp_path(&h.tmux, "host_kind"), "herdr").unwrap();
    // A Legacy probe already running when the marker landed logs before this verdict.
    kept_again(&h, "marker seen", 1).await;
    h.take_tmux_calls();
    h.append(format!("{}{}", user("T1"), said(STREAMING)).as_bytes());
    let skipped = || Harness::logged("pane capture skipped").len();
    h.until("tick with data", |h| skipped() > 0 || h.row().is_some())
        .await;
    assert!(h.row().is_none(), "no row re-acquired from an unseen pane");
    h.append(stop().as_bytes());
    h.drained("terminal frame").await;
    assert_eq!(h.take_tmux_calls(), Vec::<String>::new());
    h.cancel();
    h.exited("watcher exit").await;
}

// The abandonment check shared by the status tick and terminal preflight re-reads the host: a
// marked Herdr pane is not captured, and only the turn's own stop tombstone abandons it.
#[tokio::test]
async fn a_marked_herdr_pane_is_abandoned_only_by_its_stop_tombstone() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = crate::services::discord::host_defer_gate::tests::ScriptedTmux::install();
    let channel = ChannelId::new(6_284_190);
    let name = CLAUDE.build_tmux_session_name("p8-abandon");
    let host = HostSnapshot::new(WatchHost::Legacy);
    let abandoned = || {
        watcher_external_input_turn_abandoned(&CLAUDE, channel, &name, "/absent", 0, None, &host)
    };
    let calls = || {
        let calls = tmux.take_calls().into_iter();
        calls.filter(|call| call.contains(&name)).count()
    };
    assert!(
        abandoned(),
        "a Legacy pane that cannot be captured reads idle"
    );
    assert!(calls() > 0, "Legacy captures the pane");
    let marker = session_temp_path(&name, "host_kind");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, "herdr").unwrap();
    assert!(!abandoned(), "an unknown pane is no abandonment evidence");
    std::fs::write(session_temp_path(&name, "generation"), "p8").unwrap();
    crate::services::discord::tmux::tmux_kill_policy::record_recent_turn_stop(
        channel,
        Some(&name),
        "p8 test stop",
    )
    .await;
    assert!(abandoned(), "the turn's own stop tombstone abandons it");
    assert_eq!(calls(), 0, "no capture once the marker says Herdr");
}

// A pane only the Herdr admission map lists is never read dead by the post-stream clear and
// kill or by the missing-inflight fallback, and none of them asks tmux about it.
#[tokio::test]
async fn a_listed_herdr_pane_is_never_read_dead_after_a_stream() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = crate::services::discord::host_defer_gate::tests::ScriptedTmux::install();
    let channel = ChannelId::new(6_284_191);
    let name = CLAUDE.build_tmux_session_name("p8-listed");
    let shared = crate::services::discord::make_shared_data_for_tests();
    let host = HostSnapshot::new(WatchHost::Legacy);
    let calls = || {
        let calls = tmux.take_calls().into_iter();
        calls.filter(|call| call.contains(&name)).count()
    };
    assert!(host_gate::tmux_pane_dead(&CLAUDE, channel, &name, &host));
    assert!(calls() > 0, "a Legacy pane is probed");
    crate::services::tui_prompt_dedupe::install_herdr_execution(&name, "p8-listed");
    assert!(!host_gate::tmux_pane_dead(&CLAUDE, channel, &name, &host));
    assert!(!host_gate::tmux_dead_pane_present(
        &CLAUDE, channel, &name, &host
    ));
    assert!(host_gate::marker_alive(&shared, &name, channel, &host).await);
    assert_eq!(calls(), 0);
}
