//! The real status tick, terminal preflight and missing-inflight fallback on a session that moves
//! to Herdr mid-turn or is Herdr by its row only: no tmux call, no cleanup without a tombstone.

use std::sync::mpsc;

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::host_teardown_gate::test_support::{Stored, channel_key, seed};
use crate::services::discord::inflight::{RelayOwnerKind, TurnSource};
use crate::services::tmux_common::session_temp_path;

const T0: &str = "ADK-P8-3 T0 delivered before the watcher attached";
const S1: &str = "ADK-P8-3 S1 streamed while the row exists";
const S2: &str = "ADK-P8-3 S2 streamed when the row lapses";
const PANEL: &str = "ADK-P8-3 status panel";
const BASE: u64 = 90;
/// The separate status-panel-v2 path, whose orphan cleanup deletes the panel message.
const PANEL_V2: [(&str, &str); 2] = [
    (STATUS_PANEL_V2, "1"),
    ("AGENTDESK_SINGLE_MESSAGE_PANEL", "0"),
];

fn turn(prompt: &str, body: &str) -> String {
    format!("{}{}{}", user(prompt), said(body), stop())
}

/// A watcher past a delivered T0 whose external-input row owns a posted status panel, with
/// `body` appended as the turn the collector starts on that row.
async fn panel_turn(case: u64, body: &str) -> (Harness, serenity::MessageId) {
    let seed_text = turn("T0", T0);
    let mut h = Harness::new(case, &seed_text).await;
    let f = seed_text.len() as u64;
    h.commit(0, f);
    h.spawn(f);
    let seen = crate::services::discord::tmux_watcher_now_ms();
    h.until("started on Legacy", |h| h.heartbeat() > seen).await;
    let panel = crate::services::discord::http::send_channel_message(&h.http, h.channel, PANEL);
    let panel = panel.await.unwrap().id;
    let mut row = h.row_at(f);
    row.turn_source = TurnSource::ExternalInput;
    row.set_relay_owner_kind(RelayOwnerKind::Watcher);
    row.status_message_id = Some(panel.get());
    h.save(&row);
    h.append(body.as_bytes());
    (h, panel)
}

/// Parks the next abandonment check reached from `site`.
fn park_check_at(site: &'static str) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (paused_tx, paused_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    *panel_decisions::ABANDON_PAUSE.lock().unwrap() = Some((site, paused_tx, resume_rx));
    (paused_rx, resume_tx)
}

/// At the parked check: the row lapses, the pane reads idle to tmux, and a launch puts the
/// session on Herdr; on its own tmux snapshot the check would now drop the panel.
fn lapse_onto_herdr_while_parked(h: &Harness, paused: &mpsc::Receiver<()>) {
    paused
        .recv_timeout(Duration::from_secs(30))
        .expect("abandonment check parked");
    let root = crate::services::discord::inflight::inflight_runtime_root().unwrap();
    let row =
        crate::services::discord::inflight::inflight_state_path(&root, &CLAUDE, h.channel.get());
    std::fs::remove_file(row).unwrap();
    h.pane("idle");
    h.take_tmux_calls();
    std::fs::write(session_temp_path(&h.tmux, "host_kind"), "herdr").unwrap();
    crate::services::tui_prompt_dedupe::install_herdr_execution(&h.tmux, "p8-entry");
}

fn panel_kept(h: &Harness, panel: serenity::MessageId) -> bool {
    h.discord.lock().unwrap().visible.contains_key(&panel.get())
}

// A status tick parked at its abandonment check while the row lapses and a launch moves the
// session to Herdr re-reads the host on resume: no capture, and the panel stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_status_tick_parked_at_cleanup_rereads_the_host_before_dropping_the_panel() {
    let test = "a_status_tick_parked_at_cleanup_rereads_the_host_before_dropping_the_panel";
    if !isolated_in("herdr_entry_host_tests", test, &PANEL_V2) {
        return;
    }
    let (mut h, panel) = panel_turn(BASE, &format!("{}{}", user("T1"), said(S1))).await;
    let streamed = |h: &Harness| h.row().is_some_and(|row| row.full_response.contains(S1));
    h.until("panel turn streaming on its row", streamed).await;
    let (paused, resume) = park_check_at("streaming_status_tick");
    h.append(said(S2).as_bytes());
    lapse_onto_herdr_while_parked(&h, &paused);
    let skipped_before = Harness::logged("pane capture skipped").len();
    resume.send(()).unwrap();
    let skipped = || Harness::logged("pane capture skipped").len() > skipped_before;
    h.until("resumed check", |h| skipped() || !panel_kept(h, panel))
        .await;
    h.settle().await;
    assert!(
        panel_kept(&h, panel),
        "no cleanup without the turn's stop tombstone"
    );
    assert!(
        h.row().is_none(),
        "no row re-acquired from a pane tmux cannot see"
    );
    assert_eq!(h.take_tmux_calls(), Vec::<String>::new());
    h.cancel();
    h.exited("watcher exit").await;
}

// The terminal preflight parked at its abandonment check while the row lapses and a launch
// moves the session to Herdr re-reads the host on resume: no capture and no orphan cleanup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_preflight_parked_at_cleanup_rereads_the_host_before_dropping_the_panel() {
    let test = "a_terminal_preflight_parked_at_cleanup_rereads_the_host_before_dropping_the_panel";
    // No status tick runs in this turn, so the preflight is the first check to read the host.
    let slow_ticks = [
        PANEL_V2[0],
        PANEL_V2[1],
        ("AGENTDESK_STATUS_INTERVAL_SECS", "3600"),
    ];
    if !isolated_in("herdr_entry_host_tests", test, &slow_ticks) {
        return;
    }
    let (paused, resume) = park_check_at("terminal_preflight");
    let (mut h, panel) = panel_turn(BASE + 1, &turn("T1", S1)).await;
    lapse_onto_herdr_while_parked(&h, &paused);
    resume.send(()).unwrap();
    h.drained("terminal frame").await;
    assert!(
        panel_kept(&h, panel),
        "no orphan cleanup without the turn's stop tombstone"
    );
    assert_eq!(h.take_tmux_calls(), Vec::<String>::new());
    h.cancel();
    h.exited("watcher exit").await;
}

// A terminal committed with no inflight row on a session only its sessions row puts on Herdr
// reaches the missing-inflight fallback, which keeps the watcher instead of ending it as dead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_missing_inflight_fallback_keeps_a_herdr_session_the_pane_reads_dead_pg() {
    let test = "the_missing_inflight_fallback_keeps_a_herdr_session_the_pane_reads_dead_pg";
    if !isolated_in("herdr_entry_host_tests", test, &[]) {
        return;
    }
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let seed_text = turn("T0", T0);
    let mut h = Harness::on(BASE + 2, &seed_text, Some(pool.clone())).await;
    let key = channel_key(&h.shared, &h.tmux);
    seed(&pool, &key, &h.tmux, h.channel.get(), Stored::Hosted).await;
    let f = seed_text.len() as u64;
    h.commit(0, f);
    h.spawn(f);
    let seen = crate::services::discord::tmux_watcher_now_ms();
    h.until("started on the sessions row", |h| h.heartbeat() > seen)
        .await;
    h.pane("dead");
    h.take_tmux_calls();
    let fallbacks = || {
        let rows = crate::services::observability::metrics::snapshot().into_iter();
        let rows = rows.filter(|row| row.channel_id == h.channel.get());
        rows.map(|row| row.watcher_db_fallback_resolve_failed)
            .sum::<u64>()
    };
    assert_eq!(fallbacks(), 0);
    h.append(turn("T1", S1).as_bytes());
    h.drained("terminal frame").await;
    assert!(h.row().is_none(), "the terminal ran with no inflight row");
    assert_eq!(fallbacks(), 1, "the fallback read the session alive");
    let seen = crate::services::discord::tmux_watcher_now_ms();
    h.until("still polling", |h| h.heartbeat() > seen).await;
    assert!(
        !h.watcher_finished(),
        "the fallback did not end the watcher"
    );
    assert_eq!(h.take_tmux_calls(), Vec::<String>::new());
    h.cancel();
    h.exited("watcher exit").await;
    assert_eq!(h.kills(), 0);
    pool.close().await;
    db.drop().await;
}
