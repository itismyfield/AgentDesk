//! Terminal-success watcher / recovery start-offset helpers (#3479 item-2 split).
//!
//! Behavior-preserving extraction from `recovery_engine.rs`: the helpers that
//! decide when a terminal-success tmux pane has finished draining its JSONL
//! output (so recovery may stop watching) and that compute the byte offset a
//! restart-recovery watcher should resume reading from. They depend only on
//! `std::fs`/`tokio::time` and the tmux `has_session` / `WRAPPER_TERMINAL_END_EVENT`
//! contracts, so they live in this leaf module. The async drain driver and the
//! offset helper are re-imported by the root module so existing call sites stay
//! byte-identical.

pub(super) fn output_has_bytes_after_offset(output_path: &str, start_offset: u64) -> bool {
    std::fs::metadata(output_path)
        .map(|meta| meta.len() > start_offset)
        .unwrap_or(false)
}

const TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD: std::time::Duration = std::time::Duration::from_secs(2);

fn terminal_success_watcher_stop_allowed(
    confirmed_end: u64,
    tmux_tail_offset: u64,
    quiet_for: std::time::Duration,
) -> bool {
    confirmed_end >= tmux_tail_offset && quiet_for >= TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD
}

pub(super) async fn terminal_success_output_drained_for_recovery(
    output_path: &str,
    confirmed_end: u64,
    tmux_session_name: Option<&str>,
) -> bool {
    let Ok(before_meta) = std::fs::metadata(output_path) else {
        return false;
    };
    let tmux_alive = tmux_session_name
        .map(crate::services::platform::tmux::has_session)
        .unwrap_or(false);

    if !tmux_alive {
        return terminal_success_watcher_stop_allowed(
            confirmed_end,
            before_meta.len(),
            TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD,
        );
    }

    if !terminal_success_watcher_stop_allowed(
        confirmed_end,
        before_meta.len(),
        TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD,
    ) {
        return false;
    }

    // #2442 (H2) — fast-path: if the wrapper has already emitted the
    // `terminal_end` JSONL sentinel, the pane is *definitively* done
    // writing and we can graduate the 2s drain quiet-period immediately.
    // The wrapper writes the sentinel as one of its very last actions
    // before kill_child_tree/cleanup, so its presence is a strict superset
    // of the quiet-period heuristic. We still keep the legacy 2s sleep as
    // a fallback for SIGKILL paths that bypass the sentinel write.
    if jsonl_tail_contains_terminal_end_sentinel(output_path) {
        return terminal_success_watcher_stop_allowed(
            confirmed_end,
            before_meta.len(),
            TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD,
        );
    }

    tokio::time::sleep(TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD).await;

    let tail_after = std::fs::metadata(output_path)
        .map(|meta| meta.len())
        .unwrap_or(confirmed_end.saturating_add(1));
    tail_after == confirmed_end
        && terminal_success_watcher_stop_allowed(
            confirmed_end,
            tail_after,
            TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD,
        )
}

/// #2442 — peek the JSONL tail (last ~4 KiB) for the wrapper's
/// `terminal_end` sentinel. Reading the tail rather than the entire file
/// keeps this O(1) regardless of jsonl size. False negatives (no sentinel
/// detected when one is present) just fall back to the legacy 2s
/// quiet-period sleep, so a partial-line edge case is harmless.
fn jsonl_tail_contains_terminal_end_sentinel(output_path: &str) -> bool {
    use std::io::{Read, Seek, SeekFrom};

    const TAIL_WINDOW_BYTES: u64 = 4 * 1024;

    let Ok(mut file) = std::fs::File::open(output_path) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    let len = meta.len();
    if len == 0 {
        return false;
    }
    let start = len.saturating_sub(TAIL_WINDOW_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut buf = Vec::with_capacity(TAIL_WINDOW_BYTES as usize);
    if file.read_to_end(&mut buf).is_err() {
        return false;
    }
    // The sentinel is one JSONL line: {"type":"terminal_end",...}. We search the
    // literal `"type":"terminal_end"` token because the wrapper writes JSON via
    // `serde_json::Value::to_string()` (exact compact form); the contract lives in
    // `tmux_common::emit_wrapper_sentinel` (pretty-printing would need a rework).
    let needle = format!(
        "\"type\":\"{}\"",
        crate::services::tmux_common::WRAPPER_TERMINAL_END_EVENT
    );
    let haystack = String::from_utf8_lossy(&buf);
    haystack.contains(&needle)
}

pub(super) fn recovery_watcher_start_offset(
    output_path: &str,
    saved_last_offset: u64,
) -> (u64, u64, bool) {
    let current_len = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    if current_len >= saved_last_offset {
        (saved_last_offset, current_len, false)
    } else {
        // The output file was recreated or truncated while dcserver was down.
        // Resume from the beginning of the new file so we do not skip the
        // entire restarted session output.
        (0, current_len, true)
    }
}
