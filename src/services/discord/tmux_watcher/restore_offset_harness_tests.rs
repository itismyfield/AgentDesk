//! Runs the startup `restore_tmux_watchers` over the fake `tmux` and Discord for a live
//! Claude TUI pane whose open row a restart finds on disk.

use super::*;

const MODULE: &str = "restore_offset_harness_tests";
const OLD: [&str; 3] = [
    "ADKRR0 old turn one already delivered",
    "ADKRR0 old turn two already delivered",
    "ADKRR0 old turn three already delivered",
];
const SESSION_ID: &str = "62840000-0000-4000-8000-000000000001";

fn old_turn(body: &str) -> String {
    format!("{}{}{}", user("old"), said(body), stop())
}

/// The only live pane: busy, named for a Claude channel and launched in a workspace whose
/// transcript holds older completed turns; `wrapper` also lands them in the wrapper JSONL.
async fn restart_pane(case: u64, wrapper: bool) -> Harness {
    let mut h = Harness::new(case, "").await;
    let root = std::env::var("AGENTDESK_ROOT_DIR").unwrap();
    h.tmux = CLAUDE.build_tmux_session_name(&format!("rr0-restore-{case}-cc"));
    std::fs::write(format!("{root}/sessions"), format!("{}\n", h.tmux)).unwrap();
    let temp = |ext: &str| crate::services::tmux_common::session_temp_path(&h.tmux, ext);
    std::fs::write(temp("generation"), "restore-generation").unwrap();
    crate::services::tmux_common::write_tmux_channel_binding(&h.tmux, h.channel.get()).unwrap();
    let cwd = std::path::PathBuf::from(format!("{root}/workspace-{case}"));
    std::fs::create_dir_all(&cwd).unwrap();
    let launch = format!(
        "cd '{}'\nexec claude --session-id {SESSION_ID}\n",
        cwd.display()
    );
    let launch_ext = crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT;
    std::fs::write(temp(launch_ext), launch).unwrap();
    let home = std::env::var("CLAUDE_CONFIG_DIR").unwrap();
    let transcript = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        &cwd,
        SESSION_ID,
        Some(std::path::Path::new(&home)),
    )
    .unwrap();
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let old: String = OLD.iter().map(|body| old_turn(body)).collect();
    std::fs::write(&transcript, &old).unwrap();
    h.path = transcript.display().to_string();
    if wrapper {
        h.path = temp("jsonl");
        std::fs::write(&h.path, &old).unwrap();
    }
    h
}

/// Saves the open turn the bridge started on this pane, with no transcript offset or output
/// path recorded, after `edit`.
async fn save_bridge_row(h: &Harness, edit: impl FnOnce(&mut InflightTurnState)) {
    let send =
        |text| crate::services::discord::http::send_channel_message(&h.http, h.channel, text);
    let placeholder = send("…").await.unwrap().id;
    let mut row = InflightTurnState::new(
        CLAUDE,
        h.channel.get(),
        Some("rr0".to_owned()),
        1,
        2,
        placeholder.get(),
        "queued prompt".to_owned(),
        None,
        Some(h.tmux.clone()),
        None,
        None,
        0,
    );
    row.turn_start_offset = Some(0);
    row.readopted_from_inflight = true;
    edit(&mut row);
    h.save(&row);
}

/// Runs the startup restore and returns the offset it logged for this session's watcher.
async fn restore(h: &Harness) -> u64 {
    restore_tmux_watchers(&h.http, &h.shared).await;
    let needle = format!("Restoring tmux watcher for {} (offset ", h.tmux);
    let lines = Harness::logged(&needle);
    let line = lines.first().expect("the restore spawned a watcher");
    let at = line.split(&needle).nth(1).unwrap();
    at.split(')').next().unwrap().parse().unwrap()
}

fn shown_old_bodies(h: &Harness) -> Vec<&'static str> {
    let discord = h.discord.lock().unwrap();
    let shown = |body: &&str| discord.shown.iter().any(|c| c.contains(*body));
    OLD.iter().copied().filter(shown).collect()
}

fn claude_home() -> (tempfile::TempDir, String) {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().display().to_string();
    (home, path)
}

// A row that never recorded where it is in the fallback transcript restores at its EOF when
// nothing else is known, so none of the older turns there is posted or relayed again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_with_no_transcript_offset_restores_past_older_turns() {
    let test = "a_row_with_no_transcript_offset_restores_past_older_turns";
    let (_home, home) = claude_home();
    if !isolated_in(MODULE, test, &[("CLAUDE_CONFIG_DIR", &home)]) {
        return;
    }
    let h = restart_pane(1, false).await;
    save_bridge_row(&h, |_| {}).await;
    let old_end = h.len();
    let offset = restore(&h).await;
    h.append(old_turn("ADKRR0 the queued turn after the restart").as_bytes());
    let end = h.len();
    // The pane stays busy past a few soft-terminal debounces, then goes idle.
    tokio::time::sleep(Duration::from_secs(4)).await;
    h.pane("idle");
    h.until("read through", |h| h.read_ends().iter().any(|&r| r >= end))
        .await;
    h.settle().await;
    let frames = h.frames();
    assert_eq!(shown_old_bodies(&h), Vec::<&str>::new(), "{frames:?}");
    let replayed: Vec<_> = frames.into_iter().filter(|f| f.0 < old_end).collect();
    assert_eq!(replayed, Vec::new(), "no relay frame over the older turns");
    assert_eq!(offset, old_end);
}

// Such a row starts at the furthest position known within the transcript: a durable frontier
// inside it, else the session's Claude binding offset, else the row's turn start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_with_no_transcript_offset_restores_at_the_furthest_known_position() {
    let test = "a_row_with_no_transcript_offset_restores_at_the_furthest_known_position";
    let (_home, home) = claude_home();
    if !isolated_in(MODULE, test, &[("CLAUDE_CONFIG_DIR", &home)]) {
        return;
    }
    let one = old_turn(OLD[0]).len() as u64;
    let two = one + old_turn(OLD[1]).len() as u64;
    let bind = |h: &Harness, offset: u64| {
        let binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: h.path.clone(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(SESSION_ID.to_owned()),
            last_offset: offset,
            relay_last_offset: None,
        };
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(&h.tmux, binding);
    };

    let h = restart_pane(11, false).await;
    h.commit(0, two);
    bind(&h, one);
    save_bridge_row(&h, |row| row.turn_start_offset = Some(one)).await;
    assert_eq!(restore(&h).await, two, "the frontier end");

    let h = restart_pane(12, false).await;
    h.commit(0, h.len() + 1);
    bind(&h, two);
    save_bridge_row(&h, |row| row.turn_start_offset = Some(one)).await;
    assert_eq!(
        restore(&h).await,
        two,
        "the binding past a frontier beyond EOF"
    );

    let h = restart_pane(13, false).await;
    save_bridge_row(&h, |row| row.turn_start_offset = Some(one)).await;
    assert_eq!(restore(&h).await, one, "the turn start");
}

// A row restored over a wrapper JSONL, or one that recorded this transcript at 0, keeps 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_at_a_recorded_zero_keeps_it() {
    let test = "a_row_at_a_recorded_zero_keeps_it";
    let (_home, home) = claude_home();
    if !isolated_in(MODULE, test, &[("CLAUDE_CONFIG_DIR", &home)]) {
        return;
    }
    let h = restart_pane(21, true).await;
    save_bridge_row(&h, |_| {}).await;
    assert_eq!(restore(&h).await, 0, "the wrapper JSONL");

    let h = restart_pane(22, false).await;
    save_bridge_row(&h, |row| row.output_path = Some(h.path.clone())).await;
    assert_eq!(restore(&h).await, 0, "the recorded transcript");
}

// A watcher streaming a turn the bridge delivered, reading in chunks below a frontier that
// ends inside the transcript, suppresses that turn instead of opening it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_trailing_a_delivered_frontier_does_not_reopen_the_turn() {
    let test = "a_read_trailing_a_delivered_frontier_does_not_reopen_the_turn";
    if !isolated_in(MODULE, test, &[]) {
        return;
    }
    let pad = "x".repeat(12_000);
    let seed: String = OLD
        .iter()
        .map(|body| old_turn(&format!("{body} {pad}")))
        .collect();
    let mut h = Harness::new(31, &seed).await;
    h.commit(0, h.len());
    h.bridge_delivered = true;
    h.spawn(0);
    let suppressed = || !Harness::logged("after bridge delivered turn").is_empty();
    h.until("first streaming tick", |h| {
        suppressed() || h.row().is_some()
    })
    .await;
    h.settle().await;
    let row = h.row().map(|row| row.turn_start_offset);
    assert_eq!(row, None, "no row opened over delivered bytes");
    assert_eq!(shown_old_bodies(&h), Vec::<&str>::new());
    h.cancel();
    h.exited("watcher exit").await;
}
