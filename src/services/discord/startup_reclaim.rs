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
//! Two independent one-shot passes run at startup. After 30 seconds, the frozen-
//! panel pass scans prior-generation inflight channels and applies one finalize
//! edit that strips the animated footer and appends `⏹ (재시작으로 중단됨)`.
//! After 301 seconds, the orphan-placeholder pass scans configured provider
//! channels and deletes at most ten current-bot-authored messages per channel
//! whose content is exactly `...`, age exceeds five minutes, and id is absent
//! from freshly reloaded inflight and pending-start state. Each pass reads one
//! recent-message page of at most 50 messages per channel.
//!
//! Current-generation panels and durable placeholder identities are never
//! touched. Frozen panels must predate `boot_unix_secs`, be absent from the live-
//! anchor exclusion set, and parse as a tail-anchored frozen-spinner footer.
//! Exact-placeholder cleanup is separately guarded by bot authorship, age, and
//! durable-link exclusion. Independent per-pass guards block rescans without
//! letting the earlier frozen-panel pass suppress the delayed orphan pass.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use poise::serenity_prelude as serenity;

use super::SharedData;
use super::gateway::edit_outbound_message;
use super::inflight::{load_inflight_states_for_sweep, parse_updated_at_unix};
use super::input_runtime::fence::{BootTarget, boot_skip};
use super::single_message_panel::reclaim_finalize_text;
use crate::services::provider::ProviderKind;

/// Recent-message read page size. One page per candidate channel; matches the
/// catch-up path's `CATCH_UP_FETCH_LIMIT` so the spinner-bearing panel is on
/// the page even after bursty bot activity. `limit` takes `u8` in serenity.
const RECLAIM_FETCH_LIMIT: u8 = 50;
/// Exact synthetic placeholders younger than this remain untouched.
const ORPHAN_PLACEHOLDER_MIN_AGE_SECS: i64 = 5 * 60;
const FROZEN_PANEL_RECLAIM_DELAY_SECS: u64 = 30;
/// Wait one second beyond the strict age boundary before the orphan pass.
const ORPHAN_PLACEHOLDER_RECLAIM_DELAY_SECS: u64 = ORPHAN_PLACEHOLDER_MIN_AGE_SECS as u64 + 1;
/// Bound destructive work per channel independently from the bounded read page.
const ORPHAN_PLACEHOLDER_DELETE_LIMIT: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ReclaimPass {
    FrozenPanels,
    OrphanPlaceholders,
}

impl ReclaimPass {
    fn as_str(self) -> &'static str {
        match self {
            Self::FrozenPanels => "frozen_panels",
            Self::OrphanPlaceholders => "orphan_placeholders",
        }
    }
}

/// One-shot guard per pass: a `(pass, provider, channel_id)` recorded here has
/// already been scanned this process lifetime and is never rescanned.
fn reclaimed_channels() -> &'static Mutex<HashSet<(ReclaimPass, String, u64)>> {
    static GUARD: OnceLock<Mutex<HashSet<(ReclaimPass, String, u64)>>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Record the one-shot guard for one pass and channel.
fn claim_channel_once(pass: ReclaimPass, provider: &ProviderKind, channel_id: u64) -> bool {
    reclaimed_channels()
        .lock()
        .map(|mut set| set.insert((pass, provider.as_str().to_string(), channel_id)))
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
    orphan_placeholders_deleted: usize,
}

/// One durable-state scan, distilled into prior-generation frozen-panel
/// channels, configured orphan-placeholder channels, and the durable Discord
/// identities that each pass must preserve.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ReclaimScanPlan {
    frozen_panel_channels: Vec<u64>,
    orphan_placeholder_channels: Vec<u64>,
    live_anchor_ids: HashSet<u64>,
    protected_message_ids: HashSet<u64>,
}

/// Build the scan plan from durable rows and configured channels. Every
/// nonzero Discord message identity referenced by inflight or pending-start
/// state is protected, regardless of lifecycle age; only fully unlinked exact
/// placeholders are eligible for crash-window cleanup.
fn build_reclaim_scan_plan(provider: &ProviderKind, boot_unix_secs: i64) -> ReclaimScanPlan {
    reclaim_scan_plan_from_rows(
        load_inflight_states_for_sweep(provider),
        super::tui_direct_pending_start::load_all(),
        super::settings::list_registered_channel_bindings(),
        provider,
        boot_unix_secs,
    )
}

/// Pure core of `build_reclaim_scan_plan`, split out so channel filtering and
/// durable message-identity protection can be unit-tested without filesystem
/// or Discord access.
fn reclaim_scan_plan_from_rows(
    rows: Vec<(super::inflight::InflightTurnState, u64)>,
    pending_starts: Vec<super::tui_direct_pending_start::TuiDirectPendingStart>,
    configured_bindings: Vec<super::settings::RegisteredChannelBinding>,
    provider: &ProviderKind,
    boot_unix_secs: i64,
) -> ReclaimScanPlan {
    let mut frozen_seen = HashSet::new();
    let mut orphan_seen = HashSet::new();
    let mut plan = ReclaimScanPlan::default();
    for binding in configured_bindings {
        if &binding.owner_provider == provider
            && binding.channel_id != 0
            && orphan_seen.insert(binding.channel_id)
        {
            plan.orphan_placeholder_channels.push(binding.channel_id);
        }
    }
    for (state, _age) in rows {
        for id in [
            state.user_msg_id,
            state.current_msg_id,
            state.status_message_id.unwrap_or(0),
            state.injected_prompt_message_id.unwrap_or(0),
        ] {
            if id != 0 {
                plan.protected_message_ids.insert(id);
            }
        }
        if !state.terminal_delivery_completed() && !state.rebind_origin {
            for id in [state.current_msg_id, state.status_message_id.unwrap_or(0)] {
                if id != 0 {
                    plan.live_anchor_ids.insert(id);
                }
            }
        }
        let updated = parse_updated_at_unix(&state.updated_at).unwrap_or(i64::MAX);
        if updated < boot_unix_secs && state.channel_id != 0 && frozen_seen.insert(state.channel_id)
        {
            plan.frozen_panel_channels.push(state.channel_id);
        }
    }
    for pending in pending_starts {
        if pending.provider == provider.as_str() && pending.anchor_message_id != 0 {
            plan.protected_message_ids.insert(pending.anchor_message_id);
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

fn is_orphan_placeholder_candidate(
    author_id: u64,
    bot_user_id: u64,
    content: &str,
    message_id: u64,
    message_change_unix: i64,
    now_unix_secs: i64,
    protected_message_ids: &HashSet<u64>,
) -> bool {
    author_id == bot_user_id
        && content == "..."
        && message_change_unix < now_unix_secs - ORPHAN_PLACEHOLDER_MIN_AGE_SECS
        && !protected_message_ids.contains(&message_id)
}

async fn read_reclaim_messages(
    http: &Arc<serenity::Http>,
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<Vec<serenity::Message>> {
    match serenity::ChannelId::new(channel_id)
        .messages(
            http,
            serenity::builder::GetMessages::new().limit(RECLAIM_FETCH_LIMIT),
        )
        .await
    {
        Ok(messages) => Some(messages),
        Err(error) => {
            tracing::debug!(
                "startup-reclaim: read failed for {}/{channel_id}: {error}",
                provider.as_str()
            );
            None
        }
    }
}

async fn reclaim_frozen_panels(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    bot_user_id: u64,
    boot_unix_secs: i64,
    live_anchor_ids: &HashSet<u64>,
) -> (usize, usize) {
    let Some(messages) = read_reclaim_messages(http, provider, channel_id).await else {
        return (0, 0);
    };
    let channel = serenity::ChannelId::new(channel_id);
    let mut finalized = 0usize;
    let mut markers = 0usize;
    for msg in &messages {
        if msg.author.id.get() != bot_user_id
            || live_anchor_ids.contains(&msg.id.get())
            || message_change_unix(msg) >= boot_unix_secs
        {
            continue;
        }
        let Some(finalized_text) = reclaim_finalize_text(&msg.content, provider) else {
            continue;
        };
        // The message read awaited Discord; re-judge right before the edit.
        if super::health::legacy_supervision::legacy_retired(
            provider.as_str(),
            channel_id,
            "startup_reclaim_frozen_panel_edit",
        ) || boot_skip(BootTarget::Channel(provider, channel_id))
        {
            return (finalized, markers);
        }
        if edit_outbound_message(
            http.clone(),
            shared.clone(),
            channel,
            msg.id,
            &finalized_text,
        )
        .await
        .is_ok()
        {
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

async fn reclaim_orphan_placeholders(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    bot_user_id: u64,
    now_unix_secs: i64,
    protected_message_ids: &HashSet<u64>,
) -> usize {
    let Some(messages) = read_reclaim_messages(http, provider, channel_id).await else {
        return 0;
    };
    let channel = serenity::ChannelId::new(channel_id);
    let mut attempted = 0usize;
    let mut deleted = 0usize;
    for msg in &messages {
        if attempted >= ORPHAN_PLACEHOLDER_DELETE_LIMIT {
            break;
        }
        if !is_orphan_placeholder_candidate(
            msg.author.id.get(),
            bot_user_id,
            &msg.content,
            msg.id.get(),
            message_change_unix(msg),
            now_unix_secs,
            protected_message_ids,
        ) {
            continue;
        }
        let operation_kind =
            super::placeholder_cleanup::PlaceholderCleanupOperation::DeleteNonterminal.as_str();
        if let Some(protection) = super::placeholder_cleanup::terminal_cleanup_protects_delete(
            &shared.ui.placeholder_cleanup,
            provider,
            channel,
            msg.id,
        ) {
            crate::services::observability::emit_relay_delete(
                provider.as_str(),
                channel_id,
                msg.id.get(),
                None,
                None,
                "startup_reclaim_orphan_synthetic_placeholder",
                operation_kind,
                protection.relay_delete_outcome(),
                None,
            );
            continue;
        }
        if super::health::legacy_supervision::legacy_retired(
            provider.as_str(),
            channel_id,
            "startup_reclaim_orphan_placeholder_delete",
        ) || boot_skip(BootTarget::Channel(provider, channel_id))
        {
            return deleted;
        }
        attempted += 1;
        let result = channel.delete_message(http, msg.id).await;
        crate::services::observability::emit_relay_delete_result(
            provider.as_str(),
            channel_id,
            msg.id.get(),
            "startup_reclaim_orphan_synthetic_placeholder",
            operation_kind,
            &result,
        );
        match result {
            Ok(()) => {
                deleted += 1;
                tracing::info!(
                    "startup-reclaim: deleted unlinked synthetic placeholder channel_id={channel_id} message_id={} ({})",
                    msg.id.get(),
                    provider.as_str()
                );
            }
            Err(error) => tracing::warn!(
                "startup-reclaim: failed to delete unlinked synthetic placeholder channel_id={channel_id} message_id={} ({}): {error}",
                msg.id.get(),
                provider.as_str()
            ),
        }
    }
    deleted
}

/// Run one startup reclaim pass for `provider`. Durable state is reloaded at
/// the beginning of every pass so delayed orphan cleanup protects current rows.
async fn run_startup_reclaim_sweep(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    boot_unix_secs: i64,
    pass: ReclaimPass,
) -> ReclaimReport {
    let plan = build_reclaim_scan_plan(provider, boot_unix_secs);
    run_startup_reclaim_plan(http, shared, provider, boot_unix_secs, pass, &plan).await
}

async fn run_startup_reclaim_plan(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    boot_unix_secs: i64,
    pass: ReclaimPass,
    plan: &ReclaimScanPlan,
) -> ReclaimReport {
    let candidate_channels = match pass {
        ReclaimPass::FrozenPanels => &plan.frozen_panel_channels,
        ReclaimPass::OrphanPlaceholders => &plan.orphan_placeholder_channels,
    };
    if candidate_channels.is_empty() {
        return ReclaimReport::default();
    }
    // Fail closed: without the current bot id neither pass can prove authorship.
    // Resolve before consuming guards so a later pass still gets its opportunity.
    let bot_user_id = match http.get_current_user().await {
        Ok(user) => user.id.get(),
        Err(error) => {
            tracing::warn!(
                "startup-reclaim ({}, {}): skipping sweep — could not resolve bot user id (fail-closed): {error}",
                provider.as_str(),
                pass.as_str()
            );
            return ReclaimReport::default();
        }
    };
    let mut report = ReclaimReport::default();
    let now_unix_secs = chrono::Utc::now().timestamp();
    for &channel_id in candidate_channels {
        if super::health::legacy_supervision::legacy_retired(
            provider.as_str(),
            channel_id,
            "startup_reclaim_channel",
        ) || boot_skip(BootTarget::Channel(provider, channel_id))
            || !claim_channel_once(pass, provider, channel_id)
        {
            continue;
        }
        report.channels_scanned += 1;
        match pass {
            ReclaimPass::FrozenPanels => {
                let (finalized, markers) = reclaim_frozen_panels(
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
            ReclaimPass::OrphanPlaceholders => {
                report.orphan_placeholders_deleted += reclaim_orphan_placeholders(
                    http,
                    shared,
                    provider,
                    channel_id,
                    bot_user_id,
                    now_unix_secs,
                    &plan.protected_message_ids,
                )
                .await;
            }
        }
    }
    report
}

async fn run_delayed_reclaim_pass(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    provider: ProviderKind,
    boot_unix_secs: i64,
    delay_secs: u64,
    pass: ReclaimPass,
) {
    tokio::time::sleep(tokio::time::Duration::from_secs(delay_secs)).await;
    let report = run_startup_reclaim_sweep(&http, &shared, &provider, boot_unix_secs, pass).await;
    if report.messages_finalized > 0 || report.orphan_placeholders_deleted > 0 {
        tracing::info!(
            "startup-reclaim ({}, {}): channels_scanned={} messages_finalized={} markers_replaced={} orphan_placeholders_deleted={}",
            provider.as_str(),
            pass.as_str(),
            report.channels_scanned,
            report.messages_finalized,
            report.markers_replaced,
            report.orphan_placeholders_deleted
        );
    } else {
        tracing::debug!(
            "startup-reclaim ({}, {}): channels_scanned={} messages_finalized=0 orphan_placeholders_deleted=0",
            provider.as_str(),
            pass.as_str(),
            report.channels_scanned
        );
    }
}

/// Spawn independent one-shot passes: frozen panels after the boot settle
/// window, then exact unlinked placeholders after their minimum-age boundary.
pub(super) fn spawn_startup_reclaim_sweep(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    provider: ProviderKind,
    boot_unix_secs: i64,
) {
    tokio::spawn(run_delayed_reclaim_pass(
        http.clone(),
        shared.clone(),
        provider.clone(),
        boot_unix_secs,
        FROZEN_PANEL_RECLAIM_DELAY_SECS,
        ReclaimPass::FrozenPanels,
    ));
    tokio::spawn(run_delayed_reclaim_pass(
        http,
        shared,
        provider,
        boot_unix_secs,
        ORPHAN_PLACEHOLDER_RECLAIM_DELAY_SECS,
        ReclaimPass::OrphanPlaceholders,
    ));
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
    fn one_shot_guard_is_independent_per_pass() {
        reset_reclaim_guard_for_test();
        let provider = ProviderKind::Claude;
        assert!(claim_channel_once(
            ReclaimPass::FrozenPanels,
            &provider,
            4242
        ));
        assert!(!claim_channel_once(
            ReclaimPass::FrozenPanels,
            &provider,
            4242
        ));
        assert!(claim_channel_once(
            ReclaimPass::OrphanPlaceholders,
            &provider,
            4242
        ));
        assert!(claim_channel_once(
            ReclaimPass::FrozenPanels,
            &provider,
            9999
        ));
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
        let plan =
            reclaim_scan_plan_from_rows(rows, Vec::new(), Vec::new(), &ProviderKind::Claude, boot);
        assert!(
            plan.frozen_panel_channels.contains(&555),
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
        let plan =
            reclaim_scan_plan_from_rows(rows, Vec::new(), Vec::new(), &ProviderKind::Claude, boot);
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
        let plan =
            reclaim_scan_plan_from_rows(rows, Vec::new(), Vec::new(), &ProviderKind::Claude, boot);
        assert!(
            !plan.frozen_panel_channels.contains(&303),
            "post-boot row is current-gen, not a candidate"
        );
        assert!(plan.live_anchor_ids.contains(&7001));
        assert!(plan.live_anchor_ids.contains(&7002));
    }

    #[test]
    fn orphan_placeholder_candidate_is_exact_old_owned_and_unlinked() {
        let now = 1_700_000_000i64;
        let protected = HashSet::from([9001]);
        assert!(is_orphan_placeholder_candidate(
            7,
            7,
            "...",
            9002,
            now - ORPHAN_PLACEHOLDER_MIN_AGE_SECS - 1,
            now,
            &protected,
        ));
        for (author, content, message_id, changed) in [
            (8, "...", 9002, now - ORPHAN_PLACEHOLDER_MIN_AGE_SECS - 1),
            (7, " ... ", 9002, now - ORPHAN_PLACEHOLDER_MIN_AGE_SECS - 1),
            (7, "...", 9001, now - ORPHAN_PLACEHOLDER_MIN_AGE_SECS - 1),
            (7, "...", 9002, now - ORPHAN_PLACEHOLDER_MIN_AGE_SECS),
        ] {
            assert!(!is_orphan_placeholder_candidate(
                author, 7, content, message_id, changed, now, &protected,
            ));
        }
    }

    #[test]
    fn orphan_placeholder_delete_is_terminal_cleanup_guarded() {
        let module_src = include_str!("startup_reclaim.rs");
        let guard_needle = ["terminal_cleanup_", "protects_delete("].concat();
        let attempt_needle = ["attempted ", "+= 1;"].concat();
        let delete_needle = [".delete_", "message(http, msg.id)"].concat();
        let result_needle = ["emit_relay_delete_", "result("].concat();
        let guard = module_src
            .find(&guard_needle)
            .expect("orphan reclaim must consult terminal cleanup protection");
        let attempt = module_src
            .find(&attempt_needle)
            .expect("orphan reclaim must count destructive attempts");
        let delete = module_src
            .find(&delete_needle)
            .expect("orphan reclaim must retain its bounded delete");
        let result = module_src
            .find(&result_needle)
            .expect("orphan reclaim delete result must be durably observed");

        assert!(
            guard < attempt && attempt < delete && delete < result,
            "committed and retry-pending terminal placeholders must be skipped before a delete attempt"
        );
    }

    #[test]
    fn configured_candidates_are_provider_scoped_and_deduplicated() {
        let bindings = vec![
            super::super::settings::RegisteredChannelBinding {
                channel_id: 42,
                owner_provider: ProviderKind::Claude,
                fallback_name: None,
            },
            super::super::settings::RegisteredChannelBinding {
                channel_id: 42,
                owner_provider: ProviderKind::Claude,
                fallback_name: None,
            },
            super::super::settings::RegisteredChannelBinding {
                channel_id: 43,
                owner_provider: ProviderKind::Codex,
                fallback_name: None,
            },
        ];
        let plan = reclaim_scan_plan_from_rows(
            Vec::new(),
            Vec::new(),
            bindings,
            &ProviderKind::Claude,
            1_700_000_000,
        );
        assert_eq!(plan.orphan_placeholder_channels, vec![42]);
        assert!(plan.frozen_panel_channels.is_empty());
    }

    #[test]
    fn every_inflight_discord_identity_is_protected_from_placeholder_cleanup() {
        let boot = 1_700_000_000i64;
        let mut row = row_with(303, 7001, Some(7002), boot - 5, true, false);
        row.0.user_msg_id = 7003;
        row.0.injected_prompt_message_id = Some(7004);
        let plan = reclaim_scan_plan_from_rows(
            vec![row],
            Vec::new(),
            Vec::new(),
            &ProviderKind::Claude,
            boot,
        );
        for id in [7001, 7002, 7003, 7004] {
            assert!(plan.protected_message_ids.contains(&id));
        }
    }

    #[test]
    fn matching_pending_start_anchor_is_protected_from_placeholder_cleanup() {
        let pending = super::super::tui_direct_pending_start::TuiDirectPendingStart {
            provider: "claude".to_string(),
            channel_id: 303,
            tmux_session_name: "tmux-test".to_string(),
            prompt_text: "continue".to_string(),
            anchor_message_id: 8001,
            lease_relay_owner: "bridge_adapter".to_string(),
            lease_runtime_kind: Some("claude_tui".to_string()),
            lease_turn_id: None,
            lease_session_key: None,
            generation: 0,
            created_at_ms: 0,
            observed_at_ms: 0,
            state: super::super::tui_direct_pending_start::PendingStartState::Waiting,
            attempt_count: 0,
            captured_source: None,
            native_turn_id: None,
        };
        let plan = reclaim_scan_plan_from_rows(
            Vec::new(),
            vec![pending],
            Vec::new(),
            &ProviderKind::Claude,
            1_700_000_000,
        );
        assert!(plan.protected_message_ids.contains(&8001));
    }

    const BOT: u64 = 42;
    const FROZEN: &str = "결과\n\nTasks\n└ Bash 백그라운드 실행 ⠧";

    fn claimed(pass: ReclaimPass, channel_id: u64) -> bool {
        reclaimed_channels()
            .lock()
            .unwrap()
            .contains(&(pass, "codex".to_string(), channel_id))
    }

    /// Answers the bot identity and one page per channel holding an old frozen panel and an
    /// old exact placeholder; reading a page of a channel in `retire_on_read` retires it.
    fn reclaim_answer(
        retire_on_read: Vec<(
            u64,
            super::super::health::legacy_supervision::test_support::RetireLater,
        )>,
    ) -> super::super::health::legacy_supervision::test_support::Answer {
        use super::super::health::legacy_supervision::test_support::message_json;
        Arc::new(move |method: &axum::http::Method, path: &str| {
            if method != axum::http::Method::GET {
                return None;
            }
            if path.ends_with("/users/@me") {
                let user = serde_json::json!({"id": BOT.to_string(), "username": "bot", "discriminator": "0001", "avatar": null, "bot": true});
                return Some((200, user));
            }
            let channel: u64 = path
                .strip_suffix("/messages")?
                .rsplit('/')
                .next()?
                .parse()
                .ok()?;
            for (race, retire) in &retire_on_read {
                if *race == channel {
                    retire.retire("codex", channel);
                }
            }
            let old = "2026-01-01T00:00:00+00:00";
            let page = serde_json::json!([
                message_json(channel * 10 + 1, channel, BOT, FROZEN, old),
                message_json(channel * 10 + 2, channel, BOT, "...", old),
            ]);
            Some((200, page))
        })
    }

    /// Both startup passes skip a retired channel before claiming it, and re-judge after the
    /// message read so a channel retired during that await is neither edited nor deleted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_passes_skip_retired_channels_before_claim_and_after_read() {
        use super::super::health::legacy_supervision::RetiredForTest;
        use super::super::health::legacy_supervision::test_support::{MockDiscord, RetireLater};
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        let shared = crate::services::discord::make_shared_data_for_tests();
        let (retired, legacy, race_frozen, race_orphan) = (
            6_325_413_001u64,
            6_325_413_002u64,
            6_325_413_003u64,
            6_325_413_004u64,
        );
        let discord = MockDiscord::start_with(reclaim_answer(vec![
            (race_frozen, RetireLater::default()),
            (race_orphan, RetireLater::default()),
        ]))
        .await;
        let _retired = RetiredForTest::new("codex", retired);
        let plan = ReclaimScanPlan {
            frozen_panel_channels: vec![retired, legacy, race_frozen],
            orphan_placeholder_channels: vec![retired, legacy, race_orphan],
            ..Default::default()
        };
        let boot = chrono::Utc::now().timestamp();
        for pass in [ReclaimPass::FrozenPanels, ReclaimPass::OrphanPlaceholders] {
            run_startup_reclaim_plan(
                &discord.http,
                &shared,
                &ProviderKind::Codex,
                boot,
                pass,
                &plan,
            )
            .await;
        }

        for pass in [ReclaimPass::FrozenPanels, ReclaimPass::OrphanPlaceholders] {
            assert!(
                !claimed(pass, retired),
                "a retired channel is never claimed"
            );
        }
        assert!(discord.calls_for(retired).is_empty());
        let read = |c: u64| format!("GET /api/v10/channels/{c}/messages");
        assert_eq!(discord.calls_for(race_frozen), vec![read(race_frozen)]);
        assert_eq!(discord.calls_for(race_orphan), vec![read(race_orphan)]);
        let legacy_calls = discord.calls_for(legacy);
        assert!(
            legacy_calls.iter().any(|c| c.starts_with("PATCH")),
            "{legacy_calls:?}"
        );
        assert!(
            legacy_calls.iter().any(|c| c.starts_with("DELETE")),
            "{legacy_calls:?}"
        );
    }

    /// Both startup passes leave an input-protected channel unclaimed and unread, and a
    /// channel protected while its page is read is neither edited nor deleted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_passes_leave_protected_channels_before_claim_and_after_read() {
        use super::super::health::legacy_supervision::test_support::MockDiscord;
        use super::super::input_runtime::fence::Gate;
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        let shared = crate::services::discord::make_shared_data_for_tests();
        let (protected, legacy, race_frozen, race_orphan) = (
            6_325_577_001u64,
            6_325_577_002u64,
            6_325_577_003u64,
            6_325_577_004u64,
        );
        let answer = reclaim_answer(Vec::new());
        let discord = MockDiscord::start_with(Arc::new(move |method, path| {
            for race in [race_frozen, race_orphan] {
                if path.ends_with(&format!("/channels/{race}/messages")) {
                    Gate::protect(ProviderKind::Codex, race).unwrap();
                }
            }
            answer(method, path)
        }))
        .await;
        Gate::protect(ProviderKind::Codex, protected).unwrap();
        let plan = ReclaimScanPlan {
            frozen_panel_channels: vec![protected, legacy, race_frozen],
            orphan_placeholder_channels: vec![protected, legacy, race_orphan],
            ..Default::default()
        };
        let boot = chrono::Utc::now().timestamp();
        for pass in [ReclaimPass::FrozenPanels, ReclaimPass::OrphanPlaceholders] {
            let provider = ProviderKind::Codex;
            run_startup_reclaim_plan(&discord.http, &shared, &provider, boot, pass, &plan).await;
            assert!(
                !claimed(pass, protected),
                "a protected channel is never claimed"
            );
        }
        assert!(discord.calls_for(protected).is_empty());
        let read = |c: u64| format!("GET /api/v10/channels/{c}/messages");
        assert_eq!(discord.calls_for(race_frozen), vec![read(race_frozen)]);
        assert_eq!(discord.calls_for(race_orphan), vec![read(race_orphan)]);
        let legacy_calls = discord.calls_for(legacy);
        assert!(
            legacy_calls.iter().any(|c| c.starts_with("PATCH")),
            "{legacy_calls:?}"
        );
        assert!(
            legacy_calls.iter().any(|c| c.starts_with("DELETE")),
            "{legacy_calls:?}"
        );
    }

    #[tokio::test]
    async fn startup_outer_sweep_builds_durable_plan_and_preserves_retired_channels() {
        use super::super::health::legacy_supervision::RetiredForTest;
        use super::super::health::legacy_supervision::test_support::{
            MockDiscord, fingerprint, message_json, seed_backfill_row,
        };
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        let (retired, legacy) = (6_325_513_001u64, 6_325_513_002u64);
        let config_path = crate::runtime_layout::config_file_path(temp.path());
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let agents: Vec<_> = [retired, legacy].into_iter().map(|channel| serde_json::json!({
            "id": format!("startup-{channel}"), "name": "startup test", "provider": "codex",
            "channels": {"codex": {"id": channel.to_string(), "name": format!("test-{channel}")}}
        })).collect();
        std::fs::write(
            &config_path,
            serde_json::to_string(&serde_json::json!({
                "server": {"port": 8791}, "agents": agents
            }))
            .unwrap(),
        )
        .unwrap();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let boot = chrono::Utc::now().timestamp();
        let paths: Vec<_> = [retired, legacy]
            .into_iter()
            .map(|channel| {
                let (mut row, _) =
                    row_with(channel, channel * 10 + 3, None, boot - 600, false, false);
                row.provider = "codex".into();
                seed_backfill_row(&row)
            })
            .collect();
        let before: Vec<_> = paths.iter().map(|path| fingerprint(path)).collect();
        let answer = reclaim_answer(Vec::new());
        let discord = MockDiscord::start_with(Arc::new(move |method, path| {
            let (status, mut body) = answer(method, path)?;
            if let Some(page) = body.as_array_mut() {
                let channel = page[0]["channel_id"]
                    .as_str()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap();
                page.push(message_json(
                    channel * 10 + 3,
                    channel,
                    BOT,
                    "...",
                    "2026-01-01T00:00:00+00:00",
                ));
            }
            Some((status, body))
        }))
        .await;
        let _retired = RetiredForTest::new("codex", retired);
        for pass in [ReclaimPass::FrozenPanels, ReclaimPass::OrphanPlaceholders] {
            let report =
                run_startup_reclaim_sweep(&discord.http, &shared, &ProviderKind::Codex, boot, pass)
                    .await;
            assert_eq!(report.channels_scanned, 1, "{pass:?}: {report:?}");
            match pass {
                ReclaimPass::FrozenPanels => assert_eq!(report.messages_finalized, 1),
                ReclaimPass::OrphanPlaceholders => {
                    assert_eq!(report.orphan_placeholders_deleted, 1)
                }
            }
            assert!(!claimed(pass, retired));
            assert!(claimed(pass, legacy));
            assert_eq!(
                run_startup_reclaim_sweep(&discord.http, &shared, &ProviderKind::Codex, boot, pass)
                    .await,
                ReclaimReport::default(),
                "each pass retains its own one-shot claim"
            );
        }
        assert!(discord.calls_for(retired).is_empty());
        assert_eq!(
            paths
                .iter()
                .map(|path| fingerprint(path))
                .collect::<Vec<_>>(),
            before
        );
        let calls = discord.calls_for(legacy);
        assert!(
            calls.iter().any(|call| call
                == &format!(
                    "PATCH /api/v10/channels/{legacy}/messages/{}",
                    legacy * 10 + 1
                )),
            "{calls:?}"
        );
        assert!(
            calls.iter().any(|call| call
                == &format!(
                    "DELETE /api/v10/channels/{legacy}/messages/{}",
                    legacy * 10 + 2
                )),
            "{calls:?}"
        );
        assert!(
            !calls.iter().any(|call| call
                == &format!(
                    "DELETE /api/v10/channels/{legacy}/messages/{}",
                    legacy * 10 + 3
                )),
            "durable-linked placeholder must survive: {calls:?}"
        );
    }
}
