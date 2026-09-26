//! What main does today, losses included, for streaming turns whose row is re-acquired
//! mid-stream and the lifecycle paths a re-acquire binding would reach.

use super::*;

const T0: &str = "ADK6284 T0 delivered before the watcher attached";
const HEAD: &str = "ADK6284 T1 head streamed before the row existed";
const TAIL: &str = "ADK6284 T1 tail closing the re-acquired turn";
const NEXT: &str = "ADK6284 T2 follow-up turn body";
const DIRECT: &str = "watcher_direct";
const NONCE_MISMATCH: &str = "soft_terminal_no_authority:turn_nonce_mismatch";
const OUTSIDE_FRAME: &str = "soft_terminal_no_authority:turn_start_outside_frame";
const ADOPTED: &str = "consumed cancellation source/parser/body handoff";

fn frame(start: u64, end: u64, route: &str) -> (u64, u64, String) {
    (start, end, route.to_owned())
}

fn turn(prompt: &str, body: &str) -> String {
    format!("{}{}{}", user(prompt), said(body), stop())
}

/// A channel whose transcript holds one turn, durably delivered up to its end `F`.
async fn delivered_t0(case: u64) -> (Harness, u64) {
    let seed = turn("T0", T0);
    let h = Harness::new(case, &seed).await;
    h.commit(0, seed.len() as u64);
    (h, seed.len() as u64)
}

/// A watcher attached at `F` with no row and no binding, and T1 streaming until the
/// tick re-acquires a row at `F`.
async fn rowless_turn(case: u64) -> (Harness, u64) {
    let (mut h, f) = delivered_t0(case).await;
    h.spawn(f);
    h.append(format!("{}{}", user("T1"), said(HEAD)).as_bytes());
    h.until("streaming re-acquire", |h| h.row().is_some()).await;
    (h, f)
}

async fn finish_turn(h: &Harness, body: &str) -> u64 {
    h.append(format!("{}{}", said(body), stop()).as_bytes());
    h.drained("terminal frame").await
}

async fn next_turn(h: &Harness, body: &str) -> u64 {
    h.append(turn("next", body).as_bytes());
    h.drained("next turn frame").await
}

/// R1b: a redrive-style resume enqueued mid-stream is consumed after the terminal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rowless_turn_with_a_mid_stream_resume_baseline() {
    if !isolated("rowless_turn_with_a_mid_stream_resume_baseline") {
        return;
    }
    let (h, f) = rowless_turn(1).await;
    h.resume(f);
    let t1 = finish_turn(&h, TAIL).await;
    h.until("resume replay", |h| h.frames().len() == 2).await;
    let t2 = next_turn(&h, NEXT).await;
    let expected = Observed {
        frames: vec![
            frame(f, t1, NONCE_MISMATCH),
            frame(f, t1, DIRECT),
            frame(t1, t2, NONCE_MISMATCH),
        ],
        copies: vec![vec![1], vec![1], vec![]],
        missing_bytes: NEXT.len(),
        overwritten: 0,
        frontier: Some((f, t1)),
        row: Some((t1, false)),
    };
    assert_eq!(h.observe(&[HEAD, TAIL, NEXT]), expected);
}

/// R2b: R1b with the watcher replaced mid-stream through cancellation custody.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rowless_turn_handed_over_through_custody_baseline() {
    if !isolated("rowless_turn_handed_over_through_custody_baseline") {
        return;
    }
    let (mut h, f) = rowless_turn(2).await;
    h.resume(f);
    h.spawn(f);
    let t1 = finish_turn(&h, TAIL).await;
    let t2 = next_turn(&h, NEXT).await;
    assert_eq!(h.events(ADOPTED).len(), 0);
    let expected = Observed {
        frames: vec![frame(f, t1, DIRECT), frame(t1, t2, NONCE_MISMATCH)],
        copies: vec![vec![1], vec![1], vec![]],
        missing_bytes: NEXT.len(),
        overwritten: 0,
        frontier: Some((f, t1)),
        row: Some((t1, false)),
    };
    assert_eq!(h.observe(&[HEAD, TAIL, NEXT]), expected);

    // Control: with the row there from the start the same handover is adopted and logged.
    let (mut h, f) = delivered_t0(14).await;
    h.row_at(f);
    h.spawn(f);
    h.append(format!("{}{}", user("T1"), said(HEAD)).as_bytes());
    h.until("streaming preview", |h| h.showing(HEAD)).await;
    h.spawn(f);
    finish_turn(&h, TAIL).await;
    assert_eq!(h.events(ADOPTED).len(), 1);
}

/// R3b: another relay durably commits part, or all, of T1 before its terminal is consumed,
/// and all of it only after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rowless_turn_after_another_relay_committed_part_or_all_baseline() {
    if !isolated("rowless_turn_after_another_relay_committed_part_or_all_baseline") {
        return;
    }
    let (h, f) = rowless_turn(3).await;
    let part = h.len();
    h.commit(f, part);
    let t1 = finish_turn(&h, TAIL).await;
    let expected = Observed {
        frames: vec![frame(f, t1, NONCE_MISMATCH)],
        copies: vec![vec![1], vec![1]],
        missing_bytes: 0,
        overwritten: 0,
        frontier: Some((f, part)),
        row: Some((f, false)),
    };
    assert_eq!(h.observe(&[HEAD, TAIL]), expected, "part");
    assert_eq!(h.unowned_drops(), 1, "part");

    // The terminal line's newline is held back, so the watcher cannot consume it before the commit.
    let (h, f) = rowless_turn(4).await;
    h.append(format!("{}{}", said(TAIL), stop()).trim_end().as_bytes());
    let t1 = h.len() + 1;
    h.commit(f, t1);
    h.append(b"\n");
    h.drained("terminal frame").await;
    let expected = Observed {
        frontier: Some((f, t1)),
        ..expected
    };
    assert_eq!(h.observe(&[HEAD, TAIL]), expected, "all before");
    assert_eq!(h.unowned_drops(), 0, "all before");

    let (h, f) = rowless_turn(13).await;
    let t1 = finish_turn(&h, TAIL).await;
    h.commit(f, t1);
    assert_eq!(h.observe(&[HEAD, TAIL]), expected, "all after");
    assert_eq!(h.unowned_drops(), 1, "all after");
}

/// R6292b: a read rewound to `F` below a live row that starts at `r > F` drops
/// the undelivered turn in `[F, r)` as pre-turn bytes and commits over it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rewound_read_below_a_live_row_baseline() {
    if !isolated("rewound_read_below_a_live_row_baseline") {
        return;
    }
    const DROPPED: &str = "ADK6284 T19 body dropped before redrive";
    const LIVE: &str = "ADK6284 T20 body of the live row";
    let (t0, t19) = (turn("T0", T0), turn("T19", DROPPED));
    let mut h = Harness::new(5, &format!("{t0}{t19}{}", user("T20"))).await;
    let f = t0.len() as u64;
    h.commit(0, f);
    h.row_at(f + t19.len() as u64);
    h.spawn(f);
    let t20 = finish_turn(&h, LIVE).await;
    let expected = Observed {
        frames: vec![frame(f, t20, DIRECT)],
        copies: vec![vec![], vec![1]],
        missing_bytes: DROPPED.len(),
        overwritten: 0,
        frontier: Some((f, t20)),
        row: None,
    };
    assert_eq!(h.observe(&[DROPPED, LIVE]), expected);
}

/// A4b: a planned-drain pinned row through its own terminal and the next two turns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn planned_drain_pinned_row_baseline() {
    if !isolated("planned_drain_pinned_row_baseline") {
        return;
    }
    const PINNED: &str = "ADK6284 pinned turn body";
    const SECOND: &str = "ADK6284 turn after the pin";
    const THIRD: &str = "ADK6284 second turn after the pin";
    let (mut h, f) = delivered_t0(6).await;
    let mut row = h.row_at(f);
    row.set_restart_mode(crate::services::discord::InflightRestartMode::DrainRestart);
    h.save(&row);
    h.spawn(f);
    h.append(user("T1").as_bytes());
    let p1 = finish_turn(&h, PINNED).await;
    let p2 = next_turn(&h, SECOND).await;
    let p3 = next_turn(&h, THIRD).await;
    let expected = Observed {
        frames: vec![
            frame(f, p1, DIRECT),
            frame(p1, p2, OUTSIDE_FRAME),
            frame(p2, p3, OUTSIDE_FRAME),
        ],
        copies: vec![vec![1], vec![], vec![]],
        missing_bytes: SECOND.len() + THIRD.len(),
        overwritten: 0,
        frontier: Some((f, p1)),
        row: Some((f, false)),
    };
    assert_eq!(h.observe(&[PINNED, SECOND, THIRD]), expected);
}

/// Appends `turn` and the next turn's `head` cut `held` bytes into its first multibyte
/// scalar in one write, and returns the read end once the terminal settles.
async fn split_read(h: &Harness, turn: &str, head: &str, held: usize) -> u64 {
    let cut = head.find(|c: char| !c.is_ascii()).unwrap() + held;
    h.append(&[turn.as_bytes(), &head.as_bytes()[..cut]].concat());
    h.drained("terminal frame").await
}

/// P1-1: the terminal read ends inside a split UTF-8 scalar of the next turn's head. Pins the
/// consumed source end next to the commit end, with a row from the start and re-acquired.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn soft_terminal_read_ending_in_a_split_scalar_baseline() {
    if !isolated("soft_terminal_read_ending_in_a_split_scalar_baseline") {
        return;
    }
    const BODY: &str = "ADK6284 turn committed at the split read";
    const SPLIT: &str = "ADK6284 다음 턴 머리";
    const WITNESS: &str = "ADK6284 T3 read after the split turn";
    let head = turn("T2", SPLIT);
    let held = 2;
    let rest = head.find('다').unwrap() + held;

    let (mut h, f) = delivered_t0(7).await;
    h.row_at(f);
    h.spawn(f);
    let read_end = split_read(&h, &turn("T1", BODY), &head, held).await;
    let consumed_end = read_end - rest as u64;
    assert_eq!((consumed_end, read_end), (478, 577));
    let at_split = Observed {
        frames: vec![frame(f, read_end, DIRECT)],
        copies: vec![vec![1]],
        missing_bytes: 0,
        overwritten: 0,
        frontier: Some((f, consumed_end + held as u64)),
        row: Some((read_end, false)),
    };
    assert_eq!(h.observe(&[BODY]), at_split, "row from the start");
    // T3 shows whether the turn after the lost T2 is still read and delivered.
    h.append(&[&head.as_bytes()[rest..], turn("T3", WITNESS).as_bytes()].concat());
    h.drained("witness read").await;
    let after = Observed {
        copies: vec![vec![1], vec![], vec![]],
        missing_bytes: SPLIT.len() + WITNESS.len(),
        ..at_split
    };
    let seen = h.observe(&[BODY, SPLIT, WITNESS]);
    assert_eq!(seen, after, "row from the start, T3");

    let (h, f) = rowless_turn(12).await;
    let read_end = split_read(&h, &format!("{}{}", said(BODY), stop()), &head, held).await;
    let consumed_end = read_end - rest as u64;
    assert_eq!((consumed_end, read_end), (615, 714));
    let at_split = Observed {
        frames: vec![frame(f, read_end, NONCE_MISMATCH)],
        copies: vec![vec![1], vec![1]],
        missing_bytes: 0,
        overwritten: 0,
        frontier: Some((0, f)),
        row: Some((f, false)),
    };
    assert_eq!(h.observe(&[HEAD, BODY]), at_split, "re-acquire shape");
    h.append(&head.as_bytes()[rest..]);
    let t2 = h.drained("next turn frame").await;
    let after = Observed {
        frames: vec![
            frame(f, read_end, NONCE_MISMATCH),
            frame(read_end, t2, OUTSIDE_FRAME),
        ],
        copies: vec![vec![1], vec![1], vec![2]],
        ..at_split
    };
    let seen = h.observe(&[HEAD, BODY, SPLIT]);
    assert_eq!(seen, after, "re-acquire shape, T2");
}

/// P1-2: pane death while a bound turn streams, after its row was replaced by one with
/// the same identity axes and another (or no) nonce. Pins which clear removes the successor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pane_death_clear_against_a_same_identity_successor_baseline() {
    if !isolated("pane_death_clear_against_a_same_identity_successor_baseline") {
        return;
    }
    const STREAMING: &str = "ADK6284 bound turn streaming when the pane dies";
    const DONE: Option<&str> = Some("turn completed");
    const OTHER: Option<&str> = Some("successor-nonce");
    let mut removals = Vec::new();
    for (case, exit_reason, successor) in [
        (8, None, OTHER),
        (9, None, None),
        (10, DONE, OTHER),
        (11, DONE, None),
    ] {
        let (mut h, f) = delivered_t0(case).await;
        let row = h.row_at(f);
        h.spawn(f);
        h.append(format!("{}{}", user("T1"), said(STREAMING)).as_bytes());
        h.until("streaming preview", |h| h.showing(STREAMING)).await;
        let mut replacement = row.clone();
        replacement.turn_nonce = successor.map(str::to_owned);
        assert!(row.turn_nonce.is_some() && row.turn_nonce != replacement.turn_nonce);
        h.save(&replacement);
        if let Some(reason) = exit_reason {
            crate::services::tmux_diagnostics::record_tmux_exit_reason(&h.tmux, reason);
        }
        h.pane("dead");
        h.until("watcher exit", Harness::watcher_finished).await;
        let removed = h.events("inflight state row removal");
        let removed_by: Vec<_> = removed.iter().filter_map(|l| field(l, "reason")).collect();
        removals.push((exit_reason, successor, removed_by, h.row().is_some()));
    }
    // An abnormal death clears through the restart handoff without any identity; a normal
    // exit reaches the pane-dead clear, which matches identity axes and passes no nonce.
    let unconditional = || vec!["clear_inflight_state".to_owned()];
    let identity_only = || vec!["clear_inflight_state_if_matches_identity".to_owned()];
    assert_eq!(
        removals,
        vec![
            (None, OTHER, unconditional(), false),
            (None, None, unconditional(), false),
            (DONE, OTHER, identity_only(), false),
            (DONE, None, identity_only(), false),
        ]
    );
}
