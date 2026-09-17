//! #5452 R2: two backstops for a rotation that can't be made to work — realigning
//! the relay frontier after a shrink (L4'), and reporting a cap no longer enforced
//! (L4). Neither ever forces a rewrite: a forced rotation is the one shape that
//! manufactures loss, so the answer to "the gate never opens" is evidence for an
//! operator, not a rewrite taken anyway.

use super::*;
use std::collections::HashMap;

// Split out for the `tmux_watcher/**` line cap; nested so tests reach the
// sticky-flag state and ladder decision privately.
#[cfg(test)]
#[path = "backstop_tests.rs"]
mod rotation_backstop_tests;

/// Retry spacing for post-rotation frontier realignment, sized to fit one idle-jsonl relay poll.
const FRONTIER_REALIGN_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);
const FRONTIER_REALIGN_RETRIES: u32 = 10;

/// Channels whose post-rotation frontier realignment hasn't succeeded yet: the
/// `new_size` the arming rotation published, and the reset incarnation read
/// before its first attempt.
///
/// `new_size` (not a bare flag) because a `metadata(path)` re-derive would put
/// a rotation coordinate back on a path stat (PR-A forbids that). The
/// incarnation distinguishes a stale-high *this* rotation left from a frontier
/// already realigned elsewhere — re-applying `new_size` there would rewind
/// onto already-sent ranges.
#[derive(Clone, Copy)]
struct StickyFrontierRealign {
    new_size: u64,
    reset_incarnation: u64,
}

static STICKY_FRONTIER_REALIGN: LazyLock<Mutex<HashMap<ChannelId, StickyFrontierRealign>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Bring the in-memory relay frontier back under the rotated file's EOF (#5452 R2).
///
/// A shrink to 15 MiB while `confirmed_end_offset` still reads 21 MiB makes every
/// range of the surviving file look already-delivered — the idle-jsonl relay loop
/// skips such a range for good (no re-read path). The durable half self-heals (a
/// frontier past EOF reads as 0 under #4188); the in-memory half does not.
///
/// The reset can be declined (`reset_confirmed_frontier` refuses while an admitted
/// mutation owns the incarnation), so this retries within a budget smaller than the
/// 500 ms poll it races, then hands the rest to the per-tick sticky retry — this only
/// narrows the race window, it does not own it.
pub(super) async fn realign_frontier_after_rotation(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    tmux_session_name: &str,
    new_size: u64,
) {
    // Read before the first reset, so any reset landing from here on (this
    // function's own included) shows as movement to the retry loop and per-tick retry.
    let reset_incarnation = shared.relay_frontier_token(channel_id).reset_incarnation;
    reset_stale_relay_watermark_if_output_regressed(
        shared,
        channel_id,
        tmux_session_name,
        new_size,
        "jsonl_rotation",
    );
    for _ in 0..FRONTIER_REALIGN_RETRIES {
        if !frontier_is_still_regressed(shared, channel_id, new_size) {
            clear_sticky_frontier_realign(channel_id);
            return;
        }
        tokio::time::sleep(FRONTIER_REALIGN_RETRY_DELAY).await;
        // Read after the sleep, not at the loop top: a reset landing inside the 25 ms
        // window means `new_size` is now stale, so movement here ends the loop
        // without arming the sticky flag (already released by the same movement).
        if shared.relay_frontier_token(channel_id).reset_incarnation != reset_incarnation {
            clear_sticky_frontier_realign(channel_id);
            return;
        }
        reset_stale_relay_watermark_if_output_regressed(
            shared,
            channel_id,
            tmux_session_name,
            new_size,
            "jsonl_rotation",
        );
    }
    if frontier_is_still_regressed(shared, channel_id, new_size) {
        STICKY_FRONTIER_REALIGN
            .lock()
            .map(|mut sticky| {
                sticky.insert(
                    channel_id,
                    StickyFrontierRealign {
                        new_size,
                        reset_incarnation,
                    },
                )
            })
            .ok();
    } else {
        clear_sticky_frontier_realign(channel_id);
    }
}

/// Whether the frontier is still ahead of the rotated file's EOF.
///
/// This — never the reset's own return value — decides the retry loop's exit and
/// whether the sticky flag arms: `reset_stale_...` answers `false` both when
/// declined by an admitted mutation and when there's no regression at all, so
/// keying on it would arm the flag after every ordinary rotation and never
/// release it. Also requires the reset incarnation to match (see
/// [`StickyFrontierRealign`]) to distinguish this from an already-realigned frontier.
fn frontier_is_still_regressed(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    new_size: u64,
) -> bool {
    shared.committed_relay_offset(channel_id) > new_size
}

fn clear_sticky_frontier_realign(channel_id: ChannelId) {
    STICKY_FRONTIER_REALIGN
        .lock()
        .map(|mut sticky| sticky.remove(&channel_id))
        .ok();
}

/// The per-tick tail of the realignment, run outside the rotation cadence so it
/// retries every 250 ms rather than every 30 s. Costs nothing while nothing is
/// armed, and at most two comparisons plus one reset while something is.
pub(super) fn retry_sticky_frontier_realign(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    tmux_session_name: &str,
) {
    let Some(armed) = STICKY_FRONTIER_REALIGN
        .lock()
        .ok()
        .and_then(|sticky| sticky.get(&channel_id).copied())
    else {
        return;
    };
    // A reset landed since `new_size` was published — nothing left to walk back,
    // and applying `new_size` now would rewind onto already-delivered ranges.
    if shared.relay_frontier_token(channel_id).reset_incarnation != armed.reset_incarnation {
        clear_sticky_frontier_realign(channel_id);
        return;
    }
    if frontier_is_still_regressed(shared, channel_id, armed.new_size) {
        reset_stale_relay_watermark_if_output_regressed(
            shared,
            channel_id,
            tmux_session_name,
            armed.new_size,
            "jsonl_rotation_sticky",
        );
        if frontier_is_still_regressed(shared, channel_id, armed.new_size) {
            return;
        }
    }
    clear_sticky_frontier_realign(channel_id);
}

// ── Backstop for a cap that stops being enforced (#5452 R2, L4) ────────────
//
// The gate can refuse forever; no rotation is ever forced to stop it — a forced
// rewrite manufactures loss, which ranks above keeping the cap. What replaces
// forcing is evidence (refusal duration, dominant term, how far past cap) at
// multiples of the cap, not per tick. Consequence: a channel with no idle
// moment never gets the 20 MB cap enforced, and the file grows unbounded until
// an operator acts — deliberate, not an oversight.

const ROTATION_LADDER_WARN_MULTIPLE: u64 = 2;
const ROTATION_LADDER_ERROR_MULTIPLE: u64 = 5;

#[derive(Default)]
struct RotationRefusalLadder {
    consecutive: u32,
    /// Refusals per term this run, so the alarm names the sticky term, not the last one.
    terms: HashMap<&'static str, u32>,
    last_term: Option<RotationBusyTerm>,
    warned: bool,
    errored: bool,
}

static ROTATION_REFUSAL_LADDERS: LazyLock<Mutex<HashMap<String, RotationRefusalLadder>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RotationRefusalLevel {
    Warn,
    Error,
}

/// What the ladder decided, as fields so it's assertable and can't drift from the log.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RotationRefusalAlarm {
    level: RotationRefusalLevel,
    consecutive_refusals: u32,
    dominant_term: &'static str,
    last_term: &'static str,
    size_bytes: u64,
}

/// Fold one refusal into `ladder` and decide whether this rung is worth announcing.
///
/// Fires once per run: 2x cap warns, 5x cap errors, neither repeats until a
/// successful rotation resets the run. Reports the most-refused term alongside
/// the last one, so a run that changed character stays legible.
fn advance_rotation_refusal_ladder(
    ladder: &mut RotationRefusalLadder,
    term: RotationBusyTerm,
    size_bytes: u64,
    size_cap_bytes: u64,
) -> Option<RotationRefusalAlarm> {
    ladder.consecutive = ladder.consecutive.saturating_add(1);
    *ladder.terms.entry(term.as_str()).or_default() += 1;
    ladder.last_term = Some(term);

    let level = if size_bytes >= size_cap_bytes.saturating_mul(ROTATION_LADDER_ERROR_MULTIPLE)
        && !ladder.errored
    {
        ladder.errored = true;
        ladder.warned = true;
        RotationRefusalLevel::Error
    } else if size_bytes >= size_cap_bytes.saturating_mul(ROTATION_LADDER_WARN_MULTIPLE)
        && !ladder.warned
    {
        ladder.warned = true;
        RotationRefusalLevel::Warn
    } else {
        return None;
    };
    let dominant_term = ladder
        .terms
        .iter()
        .max_by_key(|(label, count)| (**count, **label))
        .map(|(label, _)| *label)
        .unwrap_or_else(|| term.as_str());
    Some(RotationRefusalAlarm {
        level,
        consecutive_refusals: ladder.consecutive,
        dominant_term,
        last_term: term.as_str(),
        size_bytes,
    })
}

/// Count one refusal against `output_path` and log whichever rung it reaches.
///
/// `std::fs::metadata` size feeds only the ladder's threshold/log field — no
/// rotation coordinate is derived from it (every byte coordinate stays off the
/// opened fd).
pub(super) fn record_rotation_refusal(output_path: &str, term: RotationBusyTerm) {
    let size_cap_bytes = crate::services::tmux_common::JSONL_SIZE_CAP_BYTES;
    let Ok(size_bytes) = std::fs::metadata(output_path).map(|metadata| metadata.len()) else {
        return;
    };
    if size_bytes < size_cap_bytes.saturating_mul(ROTATION_LADDER_WARN_MULTIPLE) {
        // Below every rung: still counted, so a run that reaches one reports its
        // true length. This includes ticks with nothing to rotate (an under-cap
        // file is a `FdRefusal` like any other) — `consecutive_refusals` counts
        // since the last successful rotation, not since the cap was crossed.
        let _ = ROTATION_REFUSAL_LADDERS.lock().map(|mut ladders| {
            let ladder = ladders.entry(output_path.to_string()).or_default();
            advance_rotation_refusal_ladder(ladder, term, size_bytes, size_cap_bytes)
        });
        return;
    }
    let alarm = ROTATION_REFUSAL_LADDERS
        .lock()
        .ok()
        .and_then(|mut ladders| {
            let ladder = ladders.entry(output_path.to_string()).or_default();
            advance_rotation_refusal_ladder(ladder, term, size_bytes, size_cap_bytes)
        });
    let Some(alarm) = alarm else {
        return;
    };
    let ts = chrono::Local::now().format("%H:%M:%S");
    match alarm.level {
        RotationRefusalLevel::Warn => tracing::warn!(
            output_path,
            consecutive_refusals = alarm.consecutive_refusals,
            dominant_term = alarm.dominant_term,
            last_term = alarm.last_term,
            size_bytes = alarm.size_bytes,
            "  [{ts}] ⏳ jsonl rotation has not found an idle moment — the size cap is not being enforced on this file"
        ),
        RotationRefusalLevel::Error => tracing::error!(
            output_path,
            consecutive_refusals = alarm.consecutive_refusals,
            dominant_term = alarm.dominant_term,
            last_term = alarm.last_term,
            size_bytes = alarm.size_bytes,
            "  [{ts}] 🚨 jsonl rotation is persistently refused and the file keeps growing — the end of this is disk pressure, not a lost cap"
        ),
    }
}

/// Forget the refusal run for `output_path` after a rotation that actually rewrote the file.
pub(super) fn clear_rotation_refusal_ladder(output_path: &str) {
    let _ = ROTATION_REFUSAL_LADDERS
        .lock()
        .map(|mut ladders| ladders.remove(output_path));
}
