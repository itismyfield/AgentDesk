//! The real watcher loop tearing a session down at exit, on Legacy and O channels: only a
//! local tmux session whose pane tmux or the wrapper confirms dead loses its row or is killed.

use super::*;
use crate::services::tmux_common::{session_dead_marker_path, session_temp_path};

const T0: &str = "ADK-P4B2 T0 delivered before the watcher attached";
const LEGACY_BASE: u64 = 40;
const O_BASE: u64 = 50;

fn turn(prompt: &str, body: &str) -> String {
    format!("{}{}{}", user(prompt), said(body), stop())
}

fn o_channels() -> String {
    let ids: Vec<String> = (O_BASE..O_BASE + 6)
        .map(|case| format!("[{},\"claude_tui\"]", 6_284_100 + case))
        .collect();
    format!("[{}]", ids.join(","))
}

/// A watcher attached past a delivered T0 with its row, on an O-bound session when `o`.
/// `host` is a `.host_kind` marker, or `locator` for a row bound to a Herdr pane.
async fn attached(
    case: u64,
    o: bool,
    host: Option<&str>,
) -> (
    Harness,
    Option<crate::services::tui_o::cutover::test_override::TuiSessionGuard>,
) {
    let seed = turn("T0", T0);
    let mut h = Harness::new(case, &seed).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    let bound = o.then(|| {
        crate::services::tui_o::cutover::test_override::bind_claude_tui_session(&h.tmux, &h.path)
    });
    if let Some(host) = host.filter(|host| *host != "locator") {
        std::fs::write(session_temp_path(&h.tmux, "host_kind"), host).unwrap();
    }
    crate::services::tmux_diagnostics::record_tmux_exit_reason(&h.tmux, "turn completed");
    let row = h.row_at(f);
    if host == Some("locator") {
        let mut bound = serde_json::to_value(&row).unwrap();
        bound["hosted_record_id"] = serde_json::json!(7);
        bound["hosted_execution_nonce"] = serde_json::json!("p4b2-nonce");
        bound["host_locator"] =
            serde_json::json!({"host_kind": "herdr", "host_session_id": "w1", "pane": "w1-1"});
        h.save(&serde_json::from_value(bound).unwrap());
        assert!(
            h.row().unwrap().host_locator.is_some(),
            "the binding is stored"
        );
    }
    h.spawn(f);
    (h, bound)
}

#[derive(Clone, Copy)]
enum End {
    Death,
    Cancel,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_exit_tears_down_only_a_confirmed_local_tmux_death() {
    let channels = o_channels();
    let o = crate::services::tui_o::cutover::test_override::CHANNELS_ENV;
    let flag = crate::services::tui_o::cutover::test_override::CHILD_ENV;
    if !isolated_in(
        "post_stream_exit_host_tests",
        "watcher_exit_tears_down_only_a_confirmed_local_tmux_death",
        &[(flag, "1"), (o, &channels)],
    ) {
        return;
    }
    use End::{Cancel, Death};
    // (`.host_kind`, pane, `.pane_dead`, how the watcher ends, row kept, kills)
    let cases = [
        (None, "deadpane", false, Death, false, 1),
        (None, "unanswered", false, Cancel, true, 0),
        (None, "unanswered", true, Death, false, 0),
        (None, "listfail", false, Cancel, true, 0),
        (Some("herdr"), "deadpane", true, Cancel, true, 0),
        (Some("locator"), "deadpane", false, Cancel, true, 0),
    ];
    for (base, o) in [(LEGACY_BASE, false), (O_BASE, true)] {
        for (n, (host, pane, pane_dead, end, row_kept, kills)) in cases.into_iter().enumerate() {
            let (h, bound) = attached(base + n as u64, o, host).await;
            // The pane changes first: a `.pane_dead` beside a live pane is cleared as stale.
            h.pane(pane);
            if pane_dead {
                std::fs::write(session_dead_marker_path(&h.tmux), "dead").unwrap();
            }
            if let Cancel = end {
                h.cancel();
            }
            let label = format!("o={o} {host:?} {pane} pane_dead={pane_dead}");
            h.until(&label, Harness::watcher_finished).await;
            assert_eq!((h.row().is_some(), h.kills()), (row_kept, kills), "{label}");
            let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session;
            assert_eq!(binding(&h.tmux).is_some(), bound.is_some(), "{label}");
        }
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
    for (base, o) in [(LEGACY_BASE, false), (O_BASE, true)] {
        for (n, host) in [None, Some("herdr")].into_iter().enumerate() {
            let (h, _bound) = attached(base + n as u64, o, host).await;
            h.pane(if host.is_some() {
                "deadpane"
            } else {
                "unanswered"
            });
            h.append(turn("T1", "ADK-P4B2 T1 body").as_bytes());
            h.drained("terminal frame").await;
            assert!(!h.watcher_finished(), "o={o} {host:?}");
            h.cancel();
            h.until("watcher exit", Harness::watcher_finished).await;
            assert_eq!(h.kills(), 0, "o={o} {host:?}");
        }
    }
}

// The prompt-too-long and stale-resume kills reach only a local tmux session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_kills_reach_only_a_local_tmux_session() {
    if !isolated_in(
        "post_stream_exit_host_tests",
        "abort_kills_reach_only_a_local_tmux_session",
        &[],
    ) {
        return;
    }
    let result = |text: &str| {
        let line = serde_json::json!({"type": "result", "subtype": "error_during_execution",
            "is_error": true, "result": text});
        format!("{}{line}\n", user("T1"))
    };
    let prompt_too_long = result("Prompt is too long");
    let stale = result("No conversation found with session ID: adk-p4b2");
    let cases = [
        (None, &prompt_too_long, 1),
        (Some("herdr"), &prompt_too_long, 0),
        (None, &stale, 1),
        (Some("herdr"), &stale, 0),
    ];
    for (n, (host, line, kills)) in cases.into_iter().enumerate() {
        let (h, _) = attached(LEGACY_BASE + n as u64, false, host).await;
        // The next turn's frame is read only after the abort line was handled.
        h.append(format!("{line}{}", turn("T2", "ADK-P4B2 T2 body")).as_bytes());
        h.drained("next turn frame").await;
        assert_eq!(h.kills(), kills, "{host:?} {line}");
    }
}
