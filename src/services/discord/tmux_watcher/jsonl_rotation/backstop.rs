//! The two things that happen when a rotation cannot be made to work: bringing the
//! relay frontier back under a file that did shrink (L4'), and reporting a cap that
//! has stopped being enforced on one that did not (L4). Both are #5452 R2.
//!
//! Neither ever forces a rewrite. A forced rotation is the single shape that
//! manufactures loss, and the ordering this design is built on puts losing no output
//! above keeping the file under its cap — so the answer to "the gate never opens" is
//! evidence an operator can act on, not a rewrite taken anyway.

use super::*;
use std::collections::HashMap;

// Kept in its own file so this module stays inside the `tmux_watcher/**` line cap;
// a child of `backstop` rather than a sibling, so the tests reach the sticky-flag
// state and the ladder's pure decision without either being made visible wider.
#[cfg(test)]
#[path = "backstop_tests.rs"]
mod rotation_backstop_tests;

/// Retries the post-rotation frontier realignment, spaced so the whole budget fits
/// inside one idle-jsonl relay poll.
const FRONTIER_REALIGN_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);
const FRONTIER_REALIGN_RETRIES: u32 = 10;

/// Channels whose post-rotation frontier realignment has not succeeded yet, each
/// holding what the release predicate needs: the `new_size` the rotation that armed
/// it published, and the frontier reset incarnation as it read before that rotation's
/// first realignment attempt.
///
/// `new_size` and not a bare flag, because the release predicate is stated against
/// it: outside the rotation cadence there is no fresh rotation to ask, and deriving
/// a length from `metadata(path)` here would put a rotation coordinate back on a
/// path stat, which is what PR-A forbids.
///
/// The incarnation is what makes `committed > new_size` mean the stale-high *this*
/// rotation left. Seven other call sites reset the same watermark, each against a
/// length it measured itself, and delivery advances from wherever one of them landed
/// — a frontier above `new_size` for that reason is the correct one, and re-applying
/// `new_size` to it walks the frontier back over ranges that have already been sent.
/// Keyed on the incarnation rather than on "this function's own reset succeeded
/// once", because the realignment that has to be respected is usually somebody
/// else's, and this function never sees a return value from those: what every one of
/// them does share is `reset_confirmed_frontier` bumping the incarnation on success.
#[derive(Clone, Copy)]
struct StickyFrontierRealign {
    new_size: u64,
    reset_incarnation: u64,
}

static STICKY_FRONTIER_REALIGN: LazyLock<Mutex<HashMap<ChannelId, StickyFrontierRealign>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Bring the in-memory relay frontier back under the rotated file's EOF (#5452 R2).
///
/// A rotation that shrinks the file to 15 MiB while `confirmed_end_offset` still
/// reads 21 MiB leaves every range of the surviving file looking already-delivered to
/// anything consulting `committed_relay_offset` — the idle-jsonl relay loop consumes
/// such a range without sending it, and its offset advance has no re-read path, so
/// what is skipped there is skipped for good. The durable half of that decision
/// self-heals (a frontier end past EOF reads as 0 under the #4188 guard); the
/// in-memory half does not.
///
/// The reset can be declined, because `reset_confirmed_frontier` refuses while an
/// admitted frontier mutation owns the incarnation, so this retries within a budget
/// smaller than the 500 ms poll it is racing and then hands what is left to the
/// per-tick sticky retry.
///
/// This narrows the window; it does not own it. The idle loop already calls the same
/// reset itself before reading `committed`, and neither that call nor this one can
/// constrain when the other loop polls — so a poll can still cross an arbitrarily
/// short window. The structural fix is `session_relay_sink` distrusting a `committed`
/// that exceeds the file's length, which is a different slice.
pub(super) async fn realign_frontier_after_rotation(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    tmux_session_name: &str,
    new_size: u64,
) {
    // Read before the first reset, so any reset that lands from here on — this
    // function's own included — shows up as movement to the retry loop below and to
    // the per-tick retry.
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
        // The per-tick retry's release predicate, applied to this loop's own resets and
        // read after the sleep rather than at the loop top because the reset it has to
        // hold back is the one on the far side of that sleep: a reset landing inside
        // the 25 ms window leaves a frontier measured after the rewrite, with delivery
        // advancing from there, and `new_size` applied to that is a rewind onto ranges
        // already sent — so movement here ends the loop without arming the sticky flag,
        // whose own predicate the same movement has already released.
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
/// This, and never the reset's own return value, is what decides both the retry
/// loop's exit and whether the sticky flag is armed. `reset_stale_...` answers
/// `false` for two unrelated states — a frontier that was declined by an admitted
/// mutation, and one with no regression to observe at all — so keying on it would
/// arm the flag after every ordinary rotation and then never release it, since the
/// retries would keep reporting `false` for the second reason.
///
/// Being regressed against `new_size` is necessary for either retry to act and not
/// sufficient: both also require the reset incarnation to be the one this rotation read
/// before its first attempt, which is what tells this state apart from a frontier
/// somebody else has already realigned. See [`StickyFrontierRealign`].
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
/// retries every 250 ms rather than every 30 seconds.
///
/// Costs nothing while nothing is armed, and two comparisons plus at most one reset
/// while something is: the point of running it this often is to keep that window
/// short, not to keep it open.
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
    // A reset has landed since this rotation published `new_size`, so whatever the
    // frontier reads now was put there against a length measured after the rewrite.
    // There is no stale-high of this rotation's left to walk back, and `new_size`
    // applied to an already-realigned frontier is a rewind onto delivered ranges.
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

// ── Backstop for a cap that stops being enforced (#5452 R2, L4) ─────────────
//
// The gate can refuse forever, and no rotation is ever forced to make it stop: the
// forced rewrite is the one shape that manufactures loss, and loss ranks above
// keeping the cap. What replaces forcing is evidence — how long refusals have run,
// which term is producing them, and how far past the cap the file has grown — so the
// question of whether such channels exist at all is answered by data rather than by
// a threshold guessed now. The ladder speaks at multiples of the cap, not per tick.
//
// The consequence is worth stating plainly, because it is the likeliest operational
// outcome of this design: on a channel that never presents an idle moment the 20 MB
// cap is not enforced at all, and the file grows until an operator acts on one of
// these lines. Nothing here bounds that growth. How often it happens is unmeasured —
// these are the first lines that would measure it — and the channels most exposed to
// it are the busy ones, which is where a cap earns its keep. That the ladder speaks
// instead of the rotation acting is the choice, not an oversight; what it costs is
// this.

const ROTATION_LADDER_WARN_MULTIPLE: u64 = 2;
const ROTATION_LADDER_ERROR_MULTIPLE: u64 = 5;

#[derive(Default)]
struct RotationRefusalLadder {
    consecutive: u32,
    /// Refusals per term over the current run, so the alarm can name the term that
    /// is actually sticky instead of whichever one landed last.
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

/// What the ladder decided to say, as fields rather than a formatted line, so the
/// contents are assertable without a subscriber and cannot drift from the log.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RotationRefusalAlarm {
    level: RotationRefusalLevel,
    consecutive_refusals: u32,
    dominant_term: &'static str,
    last_term: &'static str,
    size_bytes: u64,
}

/// Fold one refusal into `ladder` and decide whether this is a rung worth announcing.
///
/// A rung fires once per run: crossing twice the cap warns, crossing five times the
/// cap errors, and neither repeats until a successful rotation resets the run. The
/// dominant term is the most-refused one over the run, with the last term reported
/// alongside so a run that changed character is still legible.
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
/// The size is read with `std::fs::metadata` and is used for nothing but the ladder's
/// own threshold and log field. No rotation coordinate is derived from it, so the
/// rule that every byte coordinate comes off the opened fd is untouched.
pub(super) fn record_rotation_refusal(output_path: &str, term: RotationBusyTerm) {
    let size_cap_bytes = crate::services::tmux_common::JSONL_SIZE_CAP_BYTES;
    let Ok(size_bytes) = std::fs::metadata(output_path).map(|metadata| metadata.len()) else {
        return;
    };
    if size_bytes < size_cap_bytes.saturating_mul(ROTATION_LADDER_WARN_MULTIPLE) {
        // Below every rung: still counted, so a run that reaches one reports its
        // true length rather than starting from wherever the file crossed it.
        //
        // What lands here includes the ticks with nothing to rotate at all — an
        // under-cap file is a `FdRefusal` like any other, since the truncate answers
        // the same `None` for "no work" as for "a witness disagreed". So
        // `consecutive_refusals` is the count since the last successful rotation and
        // not the count since the cap was crossed, and the term histogram carries
        // those ticks too. Both are read only by an alarm that fires above the cap;
        // separating them means the truncate distinguishing the two answers, which
        // is a change to its signature and not to this file.
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

/// Forget the refusal run for `output_path`, so the rungs are available again for
/// the next one. Called on a rotation that actually rewrote the file.
pub(super) fn clear_rotation_refusal_ladder(output_path: &str) {
    let _ = ROTATION_REFUSAL_LADDERS
        .lock()
        .map(|mut ladders| ladders.remove(output_path));
}
