//! The real watcher loop on a channel whose TUI body O posts: Legacy consumes the turn
//! without showing its body or recording delivery evidence.

use super::*;

const T0: &str = "ADK-O T0 delivered before the watcher attached";
const BODY: &str = "ADK-O T1 body that only O may post";

fn turn(prompt: &str, body: &str) -> String {
    format!("{}{}{}", user(prompt), said(body), stop())
}

/// One streamed-then-finished turn over a delivered T0, with the watcher attached at T0's end.
async fn finished_turn(case: u64, delegated: bool) -> (Harness, u64, u64) {
    let seed = turn("T0", T0);
    let mut h = Harness::new(case, &seed).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    let _bound = delegated.then(|| {
        crate::services::tui_o::cutover::test_override::bind_claude_tui_session(&h.tmux, &h.path)
    });
    h.row_at(f);
    h.spawn(f);
    h.append(turn("T1", BODY).as_bytes());
    let t1 = h.drained("terminal frame").await;
    (h, f, t1)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn o_delegated_watcher_turn_shows_no_body_and_records_no_frontier() {
    let flag = crate::services::tui_o::cutover::test_override::CHILD_ENV;
    if !isolated_in(
        "o_delegated_watcher_tests",
        "o_delegated_watcher_turn_shows_no_body_and_records_no_frontier",
        &[(flag, "1")],
    ) {
        return;
    }
    let (legacy, f, t1) = finished_turn(21, false).await;
    let seen = legacy.observe(&[BODY]);
    assert_eq!(
        (seen.copies, seen.frontier),
        (vec![vec![1]], Some((f, t1))),
        "Legacy control"
    );

    let (h, f, _) = finished_turn(22, true).await;
    let seen = h.observe(&[BODY]);
    assert_eq!(
        seen.copies,
        vec![Vec::<u64>::new()],
        "no message shows the delegated body"
    );
    assert_eq!(seen.overwritten, 0, "and none ever showed it");
    assert_eq!(
        seen.frontier,
        Some((0, f)),
        "no durable frontier for the delegated range"
    );
}
