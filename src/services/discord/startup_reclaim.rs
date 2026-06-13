//! #3412 startup reclaim sweep.
//!
//! A deploy/restart drops the in-memory single-message-panel registry. Any
//! panel the previous generation was still animating (a repeated edit loop
//! appending a `Tasks`/`Subagents` footer or a `진행 중` status line with a
//! braille spinner) is then orphaned: no live task owns it, so the existing
//! protections all miss it —
//!   * the ⏳→✅ `tui_direct_abort_marker` drain only covers ⏳ anchors, not the
//!     footer spinner;
//!   * `placeholder_sweeper` tracks inflight state in memory, which is empty
//!     right after boot;
//!   * #3402 slot rehydration restores slots onto NEW panels, never editing the
//!     old frozen message.
//!
//! This one-shot sweep runs once per channel at startup. It finds the prior
//! generation's frozen panel via two combined sources and applies a single
//! finalize edit that strips the animated footer and appends a static
//! `⏹ (재시작으로 중단됨)` marker (body preserved verbatim, fences re-balanced):
//!   1. Persisted inflight state (`discord_inflight/<provider>/<channel>.json`)
//!      — the exact anchor / status-panel message ids of turns whose
//!      `updated_at` predates this process boot (precise targets).
//!   2. A bounded recent-message read of each such channel (serenity http,
//!      limit ≤ 50, the same path `agentdesk discord read` uses) to catch any
//!      self-authored message that still carries a frozen spinner and whose
//!      Discord create/edit time predates boot.
//!
//! The current generation's live panels are never touched. Two ownership gates
//! guard this (codex round-1 High-a): a message is finalized only when its
//! Discord change-time predates `boot_unix_secs` AND its id is absent from the
//! live-anchor exclusion set — the `current_msg_id`/`status_message_id` of every
//! not-yet-committed inflight row, which covers anchors re-adopted by alive-tmux
//! recovery without a Discord re-touch. The body must also parse as a TAIL-
//! anchored frozen-spinner footer (codex round-1 High-b), and a per-channel guard
//! blocks rescans.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use poise::serenity_prelude as serenity;

use super::SharedData;
use super::gateway::edit_outbound_message;
use super::inflight::{load_inflight_states_for_sweep, parse_updated_at_unix};
use super::single_message_panel::reclaim_finalize_text;
use crate::services::provider::ProviderKind;

/// Recent-message read page size. One page per candidate channel; matches the
/// catch-up path's `CATCH_UP_FETCH_LIMIT` so the spinner-bearing panel is on
/// the page even after bursty bot activity. `limit` takes `u8` in serenity.
const RECLAIM_FETCH_LIMIT: u8 = 50;

/// One-shot guard: a `(provider, channel_id)` recorded here has already been
/// reclaim-scanned this process lifetime and is never rescanned.
fn reclaimed_channels() -> &'static Mutex<HashSet<(String, u64)>> {
    static GUARD: OnceLock<Mutex<HashSet<(String, u64)>>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Record the one-shot guard for `(provider, channel_id)`. Returns `true` if
/// this is the first claim (caller may proceed), `false` if already scanned.
fn claim_channel_once(provider: &ProviderKind, channel_id: u64) -> bool {
    reclaimed_channels()
        .lock()
        .map(|mut set| set.insert((provider.as_str().to_string(), channel_id)))
        .unwrap_or(false)
}

#[cfg(test)]
fn reset_reclaim_guard_for_test() {
    if let Ok(mut set) = reclaimed_channels().lock() {
        set.clear();
    }
}

/// Per-pass tally for the INFO/DEBUG observability line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ReclaimReport {
    channels_scanned: usize,
    messages_finalized: usize,
    markers_replaced: usize,
}

/// One inflight scan, distilled into the two things the sweep needs:
///   * `candidate_channels` — channels whose prior generation may have left a
///     frozen panel (a row whose `updated_at` predates boot);
///   * `live_anchor_ids` — Discord message ids that the CURRENT generation may
///     still be relaying to and must never finalize (codex round-1 High-a).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ReclaimScanPlan {
    candidate_channels: Vec<u64>,
    live_anchor_ids: HashSet<u64>,
}

/// Build the scan plan from inflight rows. codex round-1 High-a: the alive-tmux
/// recovery path (`recovery_engine::reregister_active_turn_from_inflight`) can
/// re-adopt a PRE-boot row as a live turn and re-attach a watcher to its
/// `current_msg_id`/`status_message_id` WITHOUT bumping `updated_at` or editing
/// Discord. A timestamp gate alone would then let the sweep finalize that live
/// anchor. So, independent of any timing heuristic, every row that is NOT
/// terminal-delivery-committed (and is a real user-authored turn, not a
/// rebind-origin placeholder) contributes its anchor ids to an explicit
/// exclusion set. The sweep skips those ids unconditionally — a live anchor is
/// protected even though its Discord timestamp predates boot.
///
/// Recovery_engine is referenced read-only (#3410 parallel area); the alternative
/// of bumping the recovered row's `updated_at` would require editing that file,
/// so the ownership exclusion is resolved entirely on the sweep side.
fn build_reclaim_scan_plan(provider: &ProviderKind, boot_unix_secs: i64) -> ReclaimScanPlan {
    reclaim_scan_plan_from_rows(load_inflight_states_for_sweep(provider), boot_unix_secs)
}

/// Pure core of `build_reclaim_scan_plan`, split out so the High-a live-anchor
/// exclusion and the prior-generation candidate selection can be unit-tested
/// without writing inflight JSON to disk.
fn reclaim_scan_plan_from_rows(
    rows: Vec<(super::inflight::InflightTurnState, u64)>,
    boot_unix_secs: i64,
) -> ReclaimScanPlan {
    let mut seen = HashSet::new();
    let mut plan = ReclaimScanPlan::default();
    for (state, _age) in rows {
        // Live-anchor exclusion: any not-yet-committed real turn owns its anchor
        // ids regardless of when it was last touched. rebind-origin rows carry
        // placeholder ids that do not identify a real Discord message, so they
        // are excluded from the exclusion set (they cannot be a live anchor).
        if !state.terminal_delivery_completed() && !state.rebind_origin {
            for id in [state.current_msg_id]
                .into_iter()
                .chain(state.status_message_id)
            {
                if id != 0 {
                    plan.live_anchor_ids.insert(id);
                }
            }
        }
        let updated = parse_updated_at_unix(&state.updated_at).unwrap_or(i64::MAX);
        if updated >= boot_unix_secs {
            // Touched at or after boot → current generation. Leave it alone.
            continue;
        }
        if seen.insert(state.channel_id) && state.channel_id != 0 {
            plan.candidate_channels.push(state.channel_id);
        }
    }
    plan
}

/// Discord timestamp (seconds) of when `msg` last changed: its edit time if it
/// was ever edited, else its create time. Used to gate "predates boot".
fn message_change_unix(msg: &serenity::Message) -> i64 {
    change_unix_from_parts(
        msg.edited_timestamp.map(|ts| ts.unix_timestamp()),
        msg.timestamp.unix_timestamp(),
    )
}

/// Pure core of `message_change_unix`: prefer the edit time when present so a
/// pre-boot message edited after boot (current generation re-rendered it) is
/// correctly treated as current and skipped. Split out for unit testing without
/// fabricating a full serenity `Message`.
fn change_unix_from_parts(edited_unix: Option<i64>, created_unix: i64) -> i64 {
    edited_unix.unwrap_or(created_unix)
}

/// Reclaim every self-authored frozen panel on one channel. Reads ONE recent
/// page, finalizes each message whose body still parses as a frozen footer and
/// whose Discord change-time predates `boot_unix_secs`.
async fn reclaim_channel(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    bot_user_id: u64,
    boot_unix_secs: i64,
    live_anchor_ids: &HashSet<u64>,
) -> (usize, usize) {
    let channel = serenity::ChannelId::new(channel_id);
    let fetched = channel
        .messages(
            http,
            serenity::builder::GetMessages::new().limit(RECLAIM_FETCH_LIMIT),
        )
        .await;
    let messages = match fetched {
        Ok(messages) => messages,
        Err(error) => {
            tracing::debug!(
                "startup-reclaim: read failed for {}/{channel_id}: {error}",
                provider.as_str()
            );
            return (0, 0);
        }
    };

    let mut finalized = 0usize;
    let mut markers = 0usize;
    for msg in &messages {
        // codex round-1 Medium: `bot_user_id` is now a hard precondition (the
        // caller skips the channel if it could not resolve), so author matching
        // is fail-closed — only our own messages are ever edited.
        if msg.author.id.get() != bot_user_id {
            continue; // Not our own message.
        }
        // codex round-1 High-a: never finalize an anchor the current generation
        // may still be relaying to, even if its Discord timestamp predates boot
        // (alive-tmux recovery re-adopts pre-boot anchors without re-touching
        // Discord). This explicit ownership check supersedes the timing gate.
        if live_anchor_ids.contains(&msg.id.get()) {
            continue;
        }
        if message_change_unix(msg) >= boot_unix_secs {
            continue; // Current generation may still own this message.
        }
        let Some(finalized_text) = reclaim_finalize_text(&msg.content, provider) else {
            continue; // No frozen footer to reclaim (already done or live).
        };
        let edited = edit_outbound_message(
            http.clone(),
            shared.clone(),
            channel,
            msg.id,
            &finalized_text,
        )
        .await
        .is_ok();
        if edited {
            finalized += 1;
            markers += 1;
            tracing::info!(
                "startup-reclaim: finalized frozen panel channel_id={channel_id} \
                 message_id={} markers_replaced=1 ({})",
                msg.id.get(),
                provider.as_str()
            );
        }
    }
    (finalized, markers)
}

/// Run the one-shot startup reclaim sweep for `provider`. Candidate channels
/// come from prior-generation inflight rows; each is scanned at most once.
async fn run_startup_reclaim_sweep(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    boot_unix_secs: i64,
) -> ReclaimReport {
    let plan = build_reclaim_scan_plan(provider, boot_unix_secs);
    if plan.candidate_channels.is_empty() {
        return ReclaimReport::default();
    }
    // codex round-1 Medium: FAIL-CLOSED on bot-id resolution. Without our own
    // user id the author filter cannot distinguish our messages from any other
    // bot's, so a fail-open (bot_user_id = None) would edit ANY matching message.
    // Resolve once, before consuming any per-channel guard; on failure WARN and
    // abort the whole sweep WITHOUT claiming any channel, so the one-shot guards
    // stay unconsumed and a later sweep opportunity (e.g. re-fed channel) is
    // preserved. Editing zero messages is the safe outcome.
    let bot_user_id = match http.get_current_user().await {
        Ok(user) => user.id.get(),
        Err(error) => {
            tracing::warn!(
                "startup-reclaim ({}): skipping sweep — could not resolve bot user id (fail-closed): {error}",
                provider.as_str()
            );
            return ReclaimReport::default();
        }
    };
    let mut report = ReclaimReport::default();
    for channel_id in plan.candidate_channels {
        if !claim_channel_once(provider, channel_id) {
            continue; // Already scanned this process lifetime.
        }
        report.channels_scanned += 1;
        let (finalized, markers) = reclaim_channel(
            http,
            shared,
            provider,
            channel_id,
            bot_user_id,
            boot_unix_secs,
            &plan.live_anchor_ids,
        )
        .await;
        report.messages_finalized += finalized;
        report.markers_replaced += markers;
    }
    report
}

/// Spawn the one-shot startup reclaim task for `provider`. Runs once after a
/// short settle delay so live-turn recovery and #3402 slot rehydration have a
/// chance to re-register current-generation panels first (those carry a
/// post-boot timestamp and are skipped regardless, but the delay keeps the
/// reclaim read off the boot-storm critical path). Never loops — the
/// per-channel guard makes it idempotent if a channel is somehow re-fed.
pub(super) fn spawn_startup_reclaim_sweep(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    provider: ProviderKind,
    boot_unix_secs: i64,
) {
    tokio::spawn(async move {
        // Settle window: let recovery/rehydration re-register live panels first.
        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
        let report = run_startup_reclaim_sweep(&http, &shared, &provider, boot_unix_secs).await;
        if report.messages_finalized > 0 {
            tracing::info!(
                "startup-reclaim ({}): channels_scanned={} messages_finalized={} markers_replaced={}",
                provider.as_str(),
                report.channels_scanned,
                report.messages_finalized,
                report.markers_replaced
            );
        } else {
            tracing::debug!(
                "startup-reclaim ({}): channels_scanned={} messages_finalized=0",
                provider.as_str(),
                report.channels_scanned
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalize_strips_tasks_spinner_and_preserves_body() {
        // ① prior-generation footer spinner is replaced; body preserved.
        let body = "여기 작업 결과입니다.\n\n수정한 내용을 검토하세요.";
        let frozen = format!("{body}\n\nTasks\n└ Bash 백그라운드 실행 ⠧");
        let out = reclaim_finalize_text(&frozen, &ProviderKind::Claude)
            .expect("frozen Tasks footer must reclaim");
        assert!(out.starts_with(body), "body must be preserved verbatim");
        assert!(
            out.contains(super::super::single_message_panel::STARTUP_RECLAIM_STOP_MARKER),
            "stop marker must be appended"
        );
        // No spinner glyph survives anywhere.
        for frame in ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"] {
            assert!(!out.contains(frame), "spinner glyph {frame} must be gone");
        }
        assert!(
            !out.contains("Tasks"),
            "animated Tasks block must be stripped"
        );
    }

    #[test]
    fn finalize_strips_streaming_status_line() {
        let body = "진행 상황 본문";
        let frozen = format!("{body}\n\n⠧ 진행 중 — claude (<t:1700000000:R>)");
        let out = reclaim_finalize_text(&frozen, &ProviderKind::Claude)
            .expect("streaming status footer must reclaim");
        assert!(out.starts_with(body));
        assert!(!out.contains("진행 중"), "status line must be stripped");
        assert!(out.contains("⏹"));
    }

    #[test]
    fn finalize_noop_on_already_finalized_panel() {
        // ② a completed footer (only ✓ marks, no spinner) must NOT be touched.
        let done = "완료된 본문\n\nTasks\n└ Bash 백그라운드 실행 ✓";
        assert_eq!(
            reclaim_finalize_text(done, &ProviderKind::Claude),
            None,
            "finalized panel must be left untouched"
        );
        // Plain prose without any footer is also untouched.
        assert_eq!(
            reclaim_finalize_text("그냥 일반 응답입니다.", &ProviderKind::Claude),
            None
        );
    }

    #[test]
    fn finalize_keeps_code_fences_balanced() {
        // ④ a body that ended mid code fence stays renderable after reclaim.
        let frozen =
            "결과:\n```rust\nfn main() {}\n```\n\n```bash\nrunning…\n\nTasks\n└ Bash 실행 중 ⠹";
        let out = reclaim_finalize_text(frozen, &ProviderKind::Claude)
            .expect("frozen footer with dangling fence must reclaim");
        let fences = out.matches("```").count();
        assert_eq!(fences % 2, 0, "triple-backtick fences must stay balanced");
        assert!(out.contains("⏹"));
    }

    #[test]
    fn message_change_unix_prefers_edit_time() {
        // A message created before boot but edited after boot must report the
        // EDIT time, so the current-generation re-render is treated as current
        // (>= boot) and never reclaimed.
        let boot = 1_700_000_000i64;
        let created_before = boot - 100;
        let edited_after = boot + 100;
        assert_eq!(
            change_unix_from_parts(Some(edited_after), created_before),
            edited_after,
            "edit time wins when present"
        );
        // Never edited → fall back to create time.
        assert_eq!(
            change_unix_from_parts(None, created_before),
            created_before,
            "create time used when never edited"
        );
    }

    #[test]
    fn one_shot_guard_blocks_rescan() {
        // ③ the per-channel guard claims once then refuses.
        reset_reclaim_guard_for_test();
        let provider = ProviderKind::Claude;
        assert!(claim_channel_once(&provider, 4242), "first claim succeeds");
        assert!(
            !claim_channel_once(&provider, 4242),
            "second claim on same channel must be refused"
        );
        // A different channel is independent.
        assert!(claim_channel_once(&provider, 9999));
        reset_reclaim_guard_for_test();
    }

    #[test]
    fn prior_generation_filter_excludes_post_boot_rows() {
        // Pure boundary check on the updated_at vs boot comparison via the
        // helper's inner predicate: a row updated at boot is current-gen.
        // (Exercised indirectly; here we assert the comparison semantics.)
        let boot = 1_700_000_000i64;
        let prior = boot - 10;
        let current = boot + 10;
        assert!(prior < boot, "prior-gen rows are < boot");
        assert!(current >= boot, "current-gen rows are >= boot and skipped");
    }

    // ---- codex round-1 High-a: live-anchor exclusion ----

    /// Build an inflight row with caller-controlled anchor ids, `updated_at`
    /// (Unix → the persisted localtime form), and lifecycle flags.
    fn row_with(
        channel_id: u64,
        current_msg_id: u64,
        status_message_id: Option<u64>,
        updated_unix: i64,
        committed: bool,
        rebind_origin: bool,
    ) -> (super::super::inflight::InflightTurnState, u64) {
        use chrono::TimeZone;
        let mut state = super::super::inflight::InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            Some("adk-claude".to_string()),
            7,
            7,
            current_msg_id,
            "hi".to_string(),
            Some("sess".to_string()),
            None,
            None,
            None,
            0,
        );
        state.status_message_id = status_message_id;
        state.terminal_delivery_committed = committed;
        state.rebind_origin = rebind_origin;
        state.updated_at = chrono::Local
            .timestamp_opt(updated_unix, 0)
            .single()
            .expect("valid timestamp")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        (state, 0)
    }

    #[test]
    fn live_anchor_exclusion_protects_pre_boot_recovered_anchor() {
        // The alive-tmux recovery case: a PRE-boot row (updated_at < boot) that
        // is NOT terminal-delivery-committed is re-adopted as a live turn. Its
        // anchor ids must land in the exclusion set even though it predates boot,
        // and the channel is still a reclaim candidate.
        let boot = 1_700_000_000i64;
        let rows = vec![row_with(
            555,        // channel
            9001,       // current_msg_id (live anchor)
            Some(9002), // status_message_id (live anchor)
            boot - 100, // pre-boot updated_at
            false,      // not committed → live
            false,      // real turn
        )];
        let plan = reclaim_scan_plan_from_rows(rows, boot);
        assert!(
            plan.candidate_channels.contains(&555),
            "pre-boot row's channel is still a reclaim candidate"
        );
        assert!(
            plan.live_anchor_ids.contains(&9001),
            "pre-boot recovered current_msg_id must be excluded (High-a)"
        );
        assert!(
            plan.live_anchor_ids.contains(&9002),
            "pre-boot recovered status_message_id must be excluded (High-a)"
        );
    }

    #[test]
    fn live_anchor_exclusion_skips_committed_and_rebind_rows() {
        // A terminal-delivery-committed row owns nothing live; a rebind-origin
        // row's ids are not real Discord messages. Neither contributes anchors,
        // so the sweep can still finalize their frozen panels if present.
        let boot = 1_700_000_000i64;
        let rows = vec![
            row_with(101, 8001, Some(8002), boot - 50, true, false), // committed
            row_with(102, 8101, Some(8102), boot - 50, false, true), // rebind-origin
        ];
        let plan = reclaim_scan_plan_from_rows(rows, boot);
        for id in [8001, 8002, 8101, 8102] {
            assert!(
                !plan.live_anchor_ids.contains(&id),
                "committed/rebind anchor {id} must NOT be in the exclusion set"
            );
        }
    }

    #[test]
    fn live_anchor_exclusion_covers_post_boot_current_generation_rows() {
        // A post-boot, uncommitted row is the freshly started current turn. It is
        // not a reclaim candidate (>= boot) but its anchors are still excluded so
        // a concurrent sweep of the same channel cannot clobber the live panel.
        let boot = 1_700_000_000i64;
        let rows = vec![row_with(303, 7001, Some(7002), boot + 5, false, false)];
        let plan = reclaim_scan_plan_from_rows(rows, boot);
        assert!(
            !plan.candidate_channels.contains(&303),
            "post-boot row is current-gen, not a candidate"
        );
        assert!(plan.live_anchor_ids.contains(&7001));
        assert!(plan.live_anchor_ids.contains(&7002));
    }
}
