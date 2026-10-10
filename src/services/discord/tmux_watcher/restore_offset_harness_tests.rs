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
const PENDING: &str = "ADKRR0 the current turn written before the restart";
const BOOTED: &str = "ADKRR0 the current turn written while the restart was booting";

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
    let Some(line) = lines.first() else {
        panic!("no watcher restored: {:?}", Harness::logged(&h.tmux));
    };
    let at = line.split(&needle).nth(1).unwrap();
    at.split(')').next().unwrap().parse().unwrap()
}

/// The boot rehydrate's binding for this pane, registered, as the registry then holds it.
fn boot_rehydrate(h: &Harness) -> (String, u64) {
    let rehydrated =
        crate::services::discord::tui_prompt_relay::rehydrated_claude_binding_for_tests;
    let seed = rehydrated(&h.tmux).expect("the boot rehydrate binding");
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(&h.tmux, seed);
    let registered = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&h.tmux)
        .expect("the registered binding");
    (registered.output_path, registered.last_offset)
}

/// Lets the restored watcher read through `end` and go quiet, then returns its relay frames.
async fn read_through(h: &Harness, end: u64) -> Vec<(u64, u64, String)> {
    // The pane stays busy past a few soft-terminal debounces, then goes idle.
    tokio::time::sleep(Duration::from_secs(4)).await;
    h.pane("idle");
    h.until("read through", |h| h.read_ends().iter().any(|&r| r >= end))
        .await;
    h.settle().await;
    h.frames()
}

/// Whether `body` ends up shown, and in how many messages.
fn shown(h: &Harness, body: &str) -> (usize, usize) {
    let seen = h.observe(&[body]);
    (seen.missing_bytes, seen.copies[0].len())
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
    let frames = read_through(&h, h.len()).await;
    assert_eq!(shown_old_bodies(&h), Vec::<&str>::new(), "{frames:?}");
    let replayed: Vec<_> = frames.into_iter().filter(|f| f.0 < old_end).collect();
    assert_eq!(replayed, Vec::new(), "no relay frame over the older turns");
    assert_eq!(offset, old_end);
}

// The current turn's tail written before the restart is still read: the boot rehydrate seeds
// the binding at that EOF, but only the delivered frontier says where the watcher resumes. A
// watcher-owned row whose turn started at the frontier then shows that tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_undelivered_tail_before_the_restart_is_read_from_the_frontier() {
    let test = "an_undelivered_tail_before_the_restart_is_read_from_the_frontier";
    let (_home, home) = claude_home();
    if !isolated_in(MODULE, test, &[("CLAUDE_CONFIG_DIR", &home)]) {
        return;
    }
    for (case, starts_at_frontier) in [(4, false), (6, true)] {
        let h = restart_pane(case, false).await;
        let delivered = h.len();
        h.commit(0, delivered);
        h.append(old_turn(PENDING).as_bytes());
        let end = h.len();
        assert_eq!(boot_rehydrate(&h), (h.path.clone(), end));
        let turn_start = if starts_at_frontier { delivered } else { 0 };
        save_bridge_row(&h, |row| {
            row.turn_start_offset = Some(turn_start);
            if starts_at_frontier {
                row.set_relay_owner_kind(
                    crate::services::discord::inflight::RelayOwnerKind::Watcher,
                );
            }
        })
        .await;
        assert_eq!(restore(&h).await, delivered, "{case}");
        let frames = read_through(&h, end).await;
        assert_eq!(shown_old_bodies(&h), Vec::<&str>::new(), "{frames:?}");
        assert!(frames.iter().all(|f| f.0 >= delivered), "{frames:?}");
        assert!(
            frames.iter().any(|f| f.0 == delivered && f.1 >= end),
            "{frames:?}"
        );
        if starts_at_frontier {
            assert_eq!(shown(&h, PENDING), (0, 1), "{frames:?}");
        }
    }
}

// With no delivery evidence, output the pane wrote after the boot rehydrate first saw its
// transcript is read from where it saw it, not skipped to the transcript's end at restore.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_written_after_the_boot_rehydrate_is_read_from_where_it_saw_the_transcript() {
    let test = "output_written_after_the_boot_rehydrate_is_read_from_where_it_saw_the_transcript";
    let (_home, home) = claude_home();
    if !isolated_in(MODULE, test, &[("CLAUDE_CONFIG_DIR", &home)]) {
        return;
    }
    let h = restart_pane(5, false).await;
    let boot = h.len();
    assert_eq!(boot_rehydrate(&h), (h.path.clone(), boot));
    save_bridge_row(&h, |_| {}).await;
    let frontier = delivered_frontier_current_generation(&CLAUDE, h.channel, &h.tmux, None);
    let checkpoint =
        crate::services::tui_prompt_dedupe::runtime_binding_resume_checkpoint(&h.tmux, &h.path);
    let turn_start = h.row().and_then(|row| row.turn_start_offset);
    assert_eq!(
        (frontier.is_some(), checkpoint, turn_start),
        (false, None, Some(0))
    );
    h.append(old_turn(BOOTED).as_bytes());
    let end = h.len();
    assert_eq!(restore(&h).await, boot);
    let frames = read_through(&h, end).await;
    assert_eq!(shown_old_bodies(&h), Vec::<&str>::new(), "{frames:?}");
    assert!(frames.iter().all(|f| f.0 >= boot), "{frames:?}");
    assert!(
        frames.iter().any(|f| f.0 == boot && f.1 >= end),
        "{frames:?}"
    );
}

// Such a row starts at the furthest position known within the transcript, among a durable
// frontier inside it, a confirmed delivery checkpoint and the row's turn start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_with_no_transcript_offset_restores_at_the_furthest_known_position() {
    let test = "a_row_with_no_transcript_offset_restores_at_the_furthest_known_position";
    let (_home, home) = claude_home();
    if !isolated_in(MODULE, test, &[("CLAUDE_CONFIG_DIR", &home)]) {
        return;
    }
    let one = old_turn(OLD[0]).len() as u64;
    let two = one + old_turn(OLD[1]).len() as u64;
    let delivered = |h: &Harness, offset: u64| {
        let advance = crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_checkpoint;
        advance(&h.tmux, &h.path, offset);
    };
    // (case, frontier end, checkpoint, turn start, expected start)
    let past_eof = u64::MAX;
    let cases = [
        (
            "the frontier past the others",
            Some(two),
            Some(one),
            one,
            two,
        ),
        (
            "a checkpoint past the frontier",
            Some(one),
            Some(two),
            0,
            two,
        ),
        (
            "a turn start past the others",
            Some(one),
            Some(one),
            two,
            two,
        ),
        (
            "a checkpoint past a frontier beyond EOF",
            Some(past_eof),
            Some(one),
            0,
            one,
        ),
        ("the turn start alone", None, None, one, one),
    ];
    for (n, (case, frontier, checkpoint, turn_start, expected)) in cases.into_iter().enumerate() {
        let h = restart_pane(11 + n as u64, false).await;
        if let Some(frontier) = frontier {
            h.commit(0, frontier.min(h.len() + 1));
        }
        if let Some(checkpoint) = checkpoint {
            delivered(&h, checkpoint);
        }
        save_bridge_row(&h, |row| row.turn_start_offset = Some(turn_start)).await;
        assert_eq!(restore(&h).await, expected, "{case}");
    }
}

// A row restored over a wrapper JSONL or a Codex rollout, or one that recorded this transcript
// at 0, keeps 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_at_a_recorded_zero_keeps_it() {
    let test = "a_row_at_a_recorded_zero_keeps_it";
    let (_home, home) = claude_home();
    let (_codex, codex) = claude_home();
    let envs = [
        ("CLAUDE_CONFIG_DIR", home.as_str()),
        ("CODEX_HOME", codex.as_str()),
    ];
    if !isolated_in(MODULE, test, &envs) {
        return;
    }
    let h = restart_pane(21, true).await;
    save_bridge_row(&h, |_| {}).await;
    assert_eq!(restore(&h).await, 0, "the wrapper JSONL");

    let h = restart_pane(22, false).await;
    save_bridge_row(&h, |row| row.output_path = Some(h.path.clone())).await;
    assert_eq!(restore(&h).await, 0, "the recorded transcript");

    let mut h = restart_pane(23, false).await;
    let codex_provider = ProviderKind::Codex;
    h.tmux = codex_provider.build_tmux_session_name("rr0-restore-23-cdx");
    let root = std::env::var("AGENTDESK_ROOT_DIR").unwrap();
    std::fs::write(format!("{root}/sessions"), format!("{}\n", h.tmux)).unwrap();
    let temp = |ext: &str| crate::services::tmux_common::session_temp_path(&h.tmux, ext);
    std::fs::write(temp("generation"), "restore-generation").unwrap();
    crate::services::tmux_common::write_tmux_channel_binding(&h.tmux, h.channel.get()).unwrap();
    let codex_session = "62840000-0000-4000-8000-0000000000c0";
    let codex_home = std::env::var("CODEX_HOME").unwrap();
    let day = std::path::Path::new(&codex_home).join("sessions/2026/10/10");
    std::fs::create_dir_all(&day).unwrap();
    h.path = day
        .join(format!("rollout-2026-10-10T00-00-00-{codex_session}.jsonl"))
        .display()
        .to_string();
    let rollout = [
        serde_json::json!({"type": "session_meta", "payload": {"id": codex_session, "cwd": root}}),
        serde_json::json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "queued prompt"}]}}),
        serde_json::json!({"type": "response_item", "payload": {"type": "message",
            "role": "assistant", "phase": "final_answer", "channel": "final",
            "content": [{"type": "output_text", "text": "ADKRR0 codex answer"}]}}),
        serde_json::json!({"type": "event_msg", "payload": {"type": "task_complete",
            "last_agent_message": "ADKRR0 codex answer"}}),
    ];
    let rollout: String = rollout.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(&h.path, &rollout).unwrap();
    h.shared.settings.write().await.provider = codex_provider.clone();
    let send =
        |text| crate::services::discord::http::send_channel_message(&h.http, h.channel, text);
    let placeholder = send("…").await.unwrap().id;
    let mut row = InflightTurnState::new(
        codex_provider,
        h.channel.get(),
        Some("rr0-cdx".to_owned()),
        1,
        2,
        placeholder.get(),
        "queued prompt".to_owned(),
        Some(codex_session.to_owned()),
        Some(h.tmux.clone()),
        None,
        None,
        0,
    );
    row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui);
    h.save(&row);
    let reads = &super::super::super::utf8_chunk_decoder::SOURCE_READS;
    *reads.lock().unwrap() = Some(Vec::new());
    assert_eq!(restore(&h).await, 0, "the Codex rollout");
    let fallback = format!("{} — codex rollout fallback {}", h.tmux, h.path);
    assert_eq!(
        Harness::logged(&fallback).len(),
        1,
        "restored over the rollout"
    );
    let first_read = || {
        let reads = reads.lock().unwrap();
        reads
            .iter()
            .flatten()
            .find(|read| read.0 == h.path)
            .cloned()
    };
    h.until("the restored watcher's first read", |_| {
        first_read().is_some()
    })
    .await;
    let (_, start, bytes) = first_read().unwrap();
    assert_eq!((start, bytes.len()), (0, rollout.len()), "the first read");
    assert_eq!(bytes, rollout.as_bytes());
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
