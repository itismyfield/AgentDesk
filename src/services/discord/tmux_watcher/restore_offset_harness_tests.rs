//! Runs the startup `restore_tmux_watchers` over the fake `tmux` and Discord for a live
//! Claude TUI pane whose open row a restart finds on disk.

use super::*;
use std::sync::mpsc;

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
    h.tmux = CLAUDE.build_tmux_session_name(&format!("rr0-restore-{case}-cc"));
    list_sessions(&[&h.tmux]);
    let temp = |ext: &str| crate::services::tmux_common::session_temp_path(&h.tmux, ext);
    std::fs::write(temp("generation"), "restore-generation").unwrap();
    crate::services::tmux_common::write_tmux_channel_binding(&h.tmux, h.channel.get()).unwrap();
    let old: String = OLD.iter().map(|body| old_turn(body)).collect();
    h.path = launch_pane(&h.tmux, case, Some(&old));
    if wrapper {
        h.path = temp("jsonl");
        std::fs::write(&h.path, &old).unwrap();
    }
    h
}

/// Writes `tmux`'s launch script into `workspace-{case}` and returns its transcript path,
/// holding `body` if given and missing otherwise.
fn launch_pane(tmux: &str, case: u64, body: Option<&str>) -> String {
    let root = std::env::var("AGENTDESK_ROOT_DIR").unwrap();
    let cwd = std::path::PathBuf::from(format!("{root}/workspace-{case}"));
    std::fs::create_dir_all(&cwd).unwrap();
    let launch = format!(
        "cd '{}'\nexec claude --session-id {SESSION_ID}\n",
        cwd.display()
    );
    let launch_ext = crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT;
    let script = crate::services::tmux_common::session_temp_path(tmux, launch_ext);
    std::fs::write(script, launch).unwrap();
    let home = std::env::var("CLAUDE_CONFIG_DIR").unwrap();
    let transcript = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        &cwd,
        SESSION_ID,
        Some(std::path::Path::new(&home)),
    )
    .unwrap();
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    if let Some(body) = body {
        std::fs::write(&transcript, body).unwrap();
    }
    transcript.display().to_string()
}

/// The tmux sessions the fake `tmux` lists from now on.
fn list_sessions(names: &[&str]) {
    let root = std::env::var("AGENTDESK_ROOT_DIR").unwrap();
    let listed: String = names.iter().map(|name| format!("{name}\n")).collect();
    std::fs::write(format!("{root}/sessions"), listed).unwrap();
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

/// A rehydrate's binding for this pane, registered, as the registry then holds it.
fn rehydrate_binding(h: &Harness) -> (String, u64) {
    let rehydrated =
        crate::services::discord::tui_prompt_relay::rehydrated_claude_binding_for_tests;
    let seed = rehydrated(&h.tmux).expect("the rehydrate binding");
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(&h.tmux, seed);
    let registered = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&h.tmux)
        .expect("the registered binding");
    (registered.output_path, registered.last_offset)
}

/// One idle relay rehydrate pass, the boot discovery while `boot` holds.
async fn rehydrate_pass(h: &Harness, boot: &Arc<AtomicBool>) {
    crate::services::discord::tui_prompt_relay::run_claude_rehydrate_pass_for_tests(
        &h.shared, boot,
    )
    .await;
}

fn boot_read_start(h: &Harness) -> Option<u64> {
    crate::services::discord::tui_prompt_relay::boot_read_start(&h.path)
}

/// Holds the next boot observation of `path` once it has read the length: the receiver gets
/// that length and the sender releases it.
fn pause_boot_observation(path: &str) -> (mpsc::Receiver<u64>, mpsc::Sender<()>) {
    let (seen_tx, seen) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    crate::services::discord::tui_prompt_relay::pause_next_boot_observation(path, move |len| {
        seen_tx.send(len).unwrap();
        let _ = released.recv();
    });
    (seen, release)
}

/// The restore offset, the frames starting before `start`, and the frames routed other than
/// the existing soft-terminal authority refusal.
type Refused = (u64, Vec<(u64, u64, String)>, Vec<(u64, u64, String)>);

fn refused_from(offset: u64, frames: &[(u64, u64, String)], start: u64) -> Refused {
    let before = frames.iter().filter(|f| f.0 < start).cloned().collect();
    let refusal = "soft_terminal_no_authority:";
    let other = frames
        .iter()
        .filter(|f| !f.2.starts_with(refusal))
        .cloned()
        .collect();
    (offset, before, other)
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

// The tail before the restart is read from the delivered frontier, not a later boot read start:
// a watcher-owned row shows it; the bridge's own row is refused only by soft-terminal authority.
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
        rehydrate_pass(&h, &Arc::new(AtomicBool::new(true))).await;
        assert_eq!(
            boot_read_start(&h),
            Some(end),
            "a boot read start past the frontier"
        );
        assert_eq!(rehydrate_binding(&h), (h.path.clone(), end));
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
        let offset = restore(&h).await;
        assert_eq!(offset, delivered, "{case}");
        let frames = read_through(&h, end).await;
        assert_eq!(shown_old_bodies(&h), Vec::<&str>::new(), "{frames:?}");
        if starts_at_frontier {
            assert!(frames.iter().all(|f| f.0 >= delivered), "{frames:?}");
            assert_eq!(shown(&h, PENDING), (0, 1), "{frames:?}");
        } else {
            let refused = refused_from(offset, &frames, delivered);
            assert_eq!(refused, (delivered, vec![], vec![]), "{frames:?}");
        }
        assert!(
            frames.iter().any(|f| f.0 == delivered && f.1 >= end),
            "{frames:?}"
        );
    }
}

// With no delivery evidence, output written after the boot pass saw the transcript is read from
// there, even when another session's lookup reads it while that observation is recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_written_after_the_boot_rehydrate_is_read_from_where_it_saw_the_transcript() {
    let test = "output_written_after_the_boot_rehydrate_is_read_from_where_it_saw_the_transcript";
    let (_home, home) = claude_home();
    if !isolated_in(MODULE, test, &[("CLAUDE_CONFIG_DIR", &home)]) {
        return;
    }
    let h = restart_pane(5, false).await;
    let boot = h.len();
    let (seen, release) = pause_boot_observation(&h.path);
    let pass = {
        let shared = h.shared.clone();
        let boot = Arc::new(AtomicBool::new(true));
        tokio::spawn(async move {
            let pass =
                crate::services::discord::tui_prompt_relay::run_claude_rehydrate_pass_for_tests;
            pass(&shared, &boot).await;
            boot.load(Ordering::Acquire)
        })
    };
    let wait = Duration::from_secs(30);
    assert_eq!(
        seen.recv_timeout(wait),
        Ok(boot),
        "the boot observation's length"
    );
    h.append(old_turn(BOOTED).as_bytes());
    let end = h.len();
    let shared = h.shared.clone();
    let lookup = tokio::task::spawn_blocking(move || {
        let claimed = crate::services::discord::tui_prompt_relay::other_session_claimed_transcripts;
        claimed(&shared, "rr0-other")
    });
    let claimed = tokio::time::timeout(wait, lookup)
        .await
        .expect("lookup while held");
    assert!(claimed.unwrap().contains(std::path::Path::new(&h.path)));
    release.send(()).unwrap();
    assert!(
        !pass.await.unwrap(),
        "the pass completed the boot discovery"
    );
    assert_eq!(boot_read_start(&h), Some(boot));
    save_bridge_row(&h, |_| {}).await;
    let frontier = delivered_frontier_current_generation(&CLAUDE, h.channel, &h.tmux, None);
    let checkpoint =
        crate::services::tui_prompt_dedupe::runtime_binding_resume_checkpoint(&h.tmux, &h.path);
    let turn_start = h.row().and_then(|row| row.turn_start_offset);
    assert_eq!(
        (frontier.is_some(), checkpoint, turn_start),
        (false, None, Some(0))
    );
    let offset = restore(&h).await;
    assert_eq!(offset, boot);
    let frames = read_through(&h, end).await;
    assert_eq!(shown_old_bodies(&h), Vec::<&str>::new(), "{frames:?}");
    assert_eq!(refused_from(offset, &frames, boot), (boot, vec![], vec![]));
    assert!(
        frames.iter().any(|f| f.0 == boot && f.1 >= end),
        "{frames:?}"
    );
}

// Two boot observations of one transcript keep the length the first read under the map lock, and
// re-observation, re-registration, a checkpoint or a periodic pass do not move it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_boot_observation_of_a_transcript_keeps_its_length() {
    let test = "the_first_boot_observation_of_a_transcript_keeps_its_length";
    let (_home, home) = claude_home();
    if !isolated_in(MODULE, test, &[("CLAUDE_CONFIG_DIR", &home)]) {
        return;
    }
    use crate::services::discord::tui_prompt_relay::{
        boot_observation_for_tests as observe, boot_read_starts_held,
    };
    let h = restart_pane(51, false).await;
    let twin = CLAUDE.build_tmux_session_name("rr0-restore-51-twin-cc");
    assert_eq!(launch_pane(&twin, 51, None), h.path, "one transcript");
    let boot = h.len();
    let (seen, release) = pause_boot_observation(&h.path);
    let first = {
        let tmux = h.tmux.clone();
        std::thread::spawn(move || observe(&tmux))
    };
    let wait = Duration::from_secs(30);
    assert_eq!(
        seen.recv_timeout(wait),
        Ok(boot),
        "the first observation's length"
    );
    assert!(
        boot_read_starts_held(),
        "the length is read under the map lock"
    );
    h.append(old_turn(BOOTED).as_bytes());
    let end = h.len();
    let (entered_tx, entered) = mpsc::channel();
    let (done_tx, done) = mpsc::channel();
    let second = std::thread::spawn(move || {
        entered_tx.send(()).unwrap();
        let found = observe(&twin);
        done_tx.send(()).unwrap();
        found
    });
    entered
        .recv_timeout(wait)
        .expect("the second observation started");
    let waiting = done.recv_timeout(Duration::from_millis(300));
    assert_eq!(
        waiting,
        Err(mpsc::RecvTimeoutError::Timeout),
        "it waits for the first"
    );
    release.send(()).unwrap();
    assert!(first.join().unwrap());
    done.recv_timeout(wait)
        .expect("the second observation finished");
    assert!(second.join().unwrap());
    assert_eq!(boot_read_start(&h), Some(boot));

    assert_eq!(rehydrate_binding(&h), (h.path.clone(), end));
    crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_checkpoint(
        &h.tmux, &h.path, end,
    );
    assert!(observe(&h.tmux));
    rehydrate_pass(&h, &Arc::new(AtomicBool::new(false))).await;
    assert_eq!(boot_read_start(&h), Some(boot));
    let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&h.tmux);
    let checkpoint =
        crate::services::tui_prompt_dedupe::runtime_binding_resume_checkpoint(&h.tmux, &h.path);
    assert_eq!(
        (binding.map(|b| b.last_offset), checkpoint),
        (Some(end), Some(end))
    );
}

// Only passes up to the first completed tmux listing observe boot read starts, even if it then
// panicked in Herdr; a transcript first seen later gets none and its row starts at EOF.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_first_completed_tmux_listing_observes_boot_read_starts() {
    let test = "only_the_first_completed_tmux_listing_observes_boot_read_starts";
    let (_home, home) = claude_home();
    if !isolated_in(MODULE, test, &[("CLAUDE_CONFIG_DIR", &home)]) {
        return;
    }
    use crate::services::discord::tui_prompt_relay::panic_next_rehydrate_pass_at as panic_at;
    let root = std::env::var("AGENTDESK_ROOT_DIR").unwrap();
    let mut h = restart_pane(61, false).await;
    let boot = Arc::new(AtomicBool::new(true));
    std::fs::write(format!("{root}/list-fails"), "").unwrap();
    rehydrate_pass(&h, &boot).await;
    assert_eq!(boot_read_start(&h), None, "a failed listing");
    std::fs::remove_file(format!("{root}/list-fails")).unwrap();

    let first = h.len();
    panic_at(Some("listed"));
    rehydrate_pass(&h, &boot).await;
    assert_eq!(panic_at(None), None, "the pass reached its panic");
    assert_eq!(
        boot_read_start(&h),
        Some(first),
        "observed before the panic"
    );

    h.append(old_turn(PENDING).as_bytes());
    let pane = |n: u64| CLAUDE.build_tmux_session_name(&format!("rr0-restore-{n}-cc"));
    let missing = launch_pane(&pane(63), 63, None);
    let late = launch_pane(&pane(64), 64, Some(&old_turn(OLD[0])));
    list_sessions(&[&h.tmux, &pane(63), &pane(64)]);
    rehydrate_pass(&h, &boot).await;
    let at = crate::services::discord::tui_prompt_relay::boot_read_start;
    let late_len = old_turn(OLD[0]).len() as u64;
    assert_eq!(boot_read_start(&h), Some(first), "a retried observation");
    assert_eq!((at(&missing), at(&late)), (None, Some(late_len)));

    let after = launch_pane(&pane(65), 65, Some(&old_turn(OLD[1])));
    list_sessions(&[&h.tmux, &pane(65)]);
    rehydrate_pass(&h, &boot).await;
    assert_eq!(at(&after), None, "first seen after a completed listing");

    let herdr = Arc::new(AtomicBool::new(true));
    let listed = launch_pane(&pane(66), 66, Some(&old_turn(OLD[2])));
    list_sessions(&[&pane(66)]);
    panic_at(Some("herdr"));
    rehydrate_pass(&h, &herdr).await;
    assert_eq!(panic_at(None), None, "the pass reached its Herdr panic");
    assert!(at(&listed).is_some());
    h = restart_pane(62, false).await;
    rehydrate_pass(&h, &herdr).await;
    assert_eq!(
        boot_read_start(&h),
        None,
        "first seen after the Herdr panic"
    );
    assert_eq!(rehydrate_binding(&h), (h.path.clone(), h.len()));
    assert_eq!(boot_read_start(&h), None, "nor does its binding");
    save_bridge_row(&h, |_| {}).await;
    h.append(old_turn(BOOTED).as_bytes());
    assert_eq!(restore(&h).await, h.len());
}

// A restore before the boot pass observes a transcript starts at its EOF; a lookup fixes no read
// start, and the one the boot pass fixes later does not move the restored watcher.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restore_before_the_boot_observation_starts_at_the_transcript_end() {
    let test = "a_restore_before_the_boot_observation_starts_at_the_transcript_end";
    let (_home, home) = claude_home();
    if !isolated_in(MODULE, test, &[("CLAUDE_CONFIG_DIR", &home)]) {
        return;
    }
    let h = restart_pane(71, false).await;
    let other = CLAUDE.build_tmux_session_name("rr0-restore-72-cc");
    launch_pane(&other, 72, Some(&old_turn(OLD[0])));
    list_sessions(&[&h.tmux, &other]);
    let shared = h.shared.clone();
    let lookup = tokio::task::spawn_blocking(move || {
        let claimed = crate::services::discord::tui_prompt_relay::other_session_claimed_transcripts;
        claimed(&shared, &other)
    });
    assert!(
        lookup
            .await
            .unwrap()
            .contains(std::path::Path::new(&h.path))
    );
    assert_eq!(boot_read_start(&h), None, "a lookup fixes no read start");
    h.append(old_turn(PENDING).as_bytes());
    save_bridge_row(&h, |_| {}).await;
    let start = h.len();
    assert_eq!(restore(&h).await, start);
    rehydrate_pass(&h, &Arc::new(AtomicBool::new(true))).await;
    assert_eq!(boot_read_start(&h), Some(start));
    h.append(old_turn(BOOTED).as_bytes());
    let frames = read_through(&h, h.len()).await;
    assert_eq!(shown_old_bodies(&h), Vec::<&str>::new(), "{frames:?}");
    assert!(frames.iter().all(|f| f.0 >= start), "{frames:?}");
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
    let polls = &super::super::super::utf8_chunk_decoder::POLLS;
    *polls.lock().unwrap() = Some(Vec::new());
    assert_eq!(restore(&h).await, 0, "the Codex rollout");
    let fallback = format!("{} — codex rollout fallback {}", h.tmux, h.path);
    assert_eq!(
        Harness::logged(&fallback).len(),
        1,
        "restored over the rollout"
    );
    let first_poll = || {
        let polls = polls.lock().unwrap();
        polls
            .iter()
            .flatten()
            .find(|poll| poll.0 == h.path)
            .cloned()
    };
    h.until("the restored watcher's first poll", |_| {
        first_poll().is_some()
    })
    .await;
    let (_, read_start, end, raw) = first_poll().unwrap();
    let whole = (0, rollout.len() as u64, rollout.as_bytes());
    assert_eq!((read_start, end, raw.as_slice()), whole, "the first poll");
    let reads = reads.lock().unwrap();
    let first_read = reads.iter().flatten().find(|read| read.0 == h.path);
    let (_, start, bytes) = first_read.expect("the first non-empty read");
    assert_eq!(
        (*start, bytes.as_slice()),
        (0, rollout.as_bytes()),
        "the first read"
    );
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
