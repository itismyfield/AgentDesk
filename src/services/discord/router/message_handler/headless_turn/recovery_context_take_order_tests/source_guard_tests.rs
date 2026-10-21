//! Classification 2: as-written, comment-free matching inside the headless entry, so prose
//! and these needles never count. A lexical tripwire; counts are not paired with awaits.

use crate::services::discord::router::source_guard_support::{Lexed, forge, swap};
use crate::test_support::CommentForgery;

pub(crate) const HEADLESS_TURN: &str = include_str!("../../headless_turn.rs");

const ENTRY: &str = "async fn start_reserved_headless_turn_admitted(";
const SPAWN: &str = "tokio::task::spawn_blocking";
const ROW_REFUSAL: &str = "foreign_row::admit_headless(shared, &provider, &inflight_state, &cancel_token, created)\n        .await?;";
const STARTED: &str = "Ok(HeadlessTurnStartOutcome::started(";
const LIFECYCLE: &str = "if let GoalCommandKind::Lifecycle(command) = headless_goal_kind";
const CONSUMED: &str = "return Ok(HeadlessTurnStartOutcome::consumed(";
const TAKE: &str =
    "let session_retry_context = take_session_retry_context(shared, channel_id, Some(&turn_id));";
/// Where the recovered context enters the prompt (`TurnContext`) and the manifest.
const PROMPT_USES: [&str; 2] = [
    "reply: reply_context.as_deref(),",
    "let recovery_context_for_manifest =",
];
const BUILDER: &str = "let built_system_prompt = build_system_prompt_with_manifest(";
const HEADLESS_PROFILE: &str = "PromptProfiles::headless(dispatch_profile)";
const SEVERANCE: &str = "let fresh_context_severance = goal_fresh || fresh_routine;";
const SNAPSHOT_SEVERANCE: &str = "goal_fresh || fresh_routine || scheduled_snapshot_turn";
const LOCAL_COLD_START: &str = "if scheduled_snapshot_turn {\n        session_id = None;";
const GUARDED_REFRESH: &str =
    "if !scheduled_snapshot_turn\n        && valid_routine_metadata(metadata.as_ref()).is_some()";

fn entry(source: &str) -> Result<Lexed, Vec<String>> {
    Lexed::new(source)
        .block(ENTRY)
        .ok_or_else(|| vec![format!("missing {ENTRY}")])
}

fn ordered(label: &str, earlier: Option<usize>, later: Option<usize>) -> Vec<String> {
    match (earlier, later) {
        (Some(earlier), Some(later)) if earlier < later => Vec::new(),
        found => vec![format!("{label}: {found:?}")],
    }
}

/// The durable-row refusal returns before any provider or bridge spawn.
pub(crate) fn row_refusal_gaps(source: &str) -> Vec<String> {
    entry(source).map_or_else(
        |gaps| gaps,
        |src| {
            ordered(
                "row refusal precedes spawn",
                src.find(ROW_REFUSAL),
                src.find(SPAWN),
            )
        },
    )
}

/// Between the provider spawn and the Started return, no retryable start error exists.
pub(crate) fn post_spawn_gaps(source: &str) -> Vec<String> {
    let src = match entry(source) {
        Ok(src) => src,
        Err(gaps) => return gaps,
    };
    let Some((spawn, started)) = src
        .find(SPAWN)
        .and_then(|spawn| Some((spawn, src.find_after(spawn, STARTED)?)))
    else {
        return vec!["headless Started return must follow the provider spawn".to_string()];
    };
    match src.text()[spawn..started].contains("HeadlessTurnStartError::") {
        true => vec!["post-spawn failures must flow through the bridge".to_string()],
        false => Vec::new(),
    }
}

/// The lifecycle branch's Consumed return comes before the destructive context take.
pub(crate) fn lifecycle_take_gaps(source: &str) -> Vec<String> {
    entry(source).map_or_else(
        |gaps| gaps,
        |src| {
            let consumed = src
                .find(LIFECYCLE)
                .and_then(|branch| src.find_after(branch, CONSUMED));
            ordered(
                "lifecycle Consumed precedes the take",
                consumed,
                src.find(TAKE),
            )
        },
    )
}

/// One destructive take, before both prompt uses of the recovered context.
pub(crate) fn take_once_gaps(source: &str) -> Vec<String> {
    let src = match entry(source) {
        Ok(src) => src,
        Err(gaps) => return gaps,
    };
    let mut gaps = Vec::new();
    if src.count(TAKE) != 1 {
        gaps.push("exactly one destructive recovery-context take".to_string());
    }
    for used in PROMPT_USES {
        gaps.extend(ordered(used, src.find(TAKE), src.find(used)));
    }
    gaps
}

/// The actual prompt assembly call selects the headless profile, never foreground.
pub(crate) fn headless_profile_gaps(source: &str) -> Vec<String> {
    let src = match entry(source) {
        Ok(src) => src,
        Err(gaps) => return gaps,
    };
    let Some(call) = src.find(BUILDER).and_then(|start| {
        let end = src.find_after(start, "\n    );")?;
        Some(src.slice(start..end))
    }) else {
        return vec!["headless prompt assembly call is missing".to_string()];
    };
    let mut gaps = Vec::new();
    if !call.contains(HEADLESS_PROFILE) {
        gaps.push(format!("assembly lost {HEADLESS_PROFILE}"));
    }
    if call.text().contains("PromptProfiles::foreground") {
        gaps.push("assembly selects the foreground profile".to_string());
    }
    gaps
}

/// Scheduled snapshots stay out of channel-clearing severance and cold-start locally.
pub(crate) fn snapshot_severance_gaps(source: &str) -> Vec<String> {
    let src = match entry(source) {
        Ok(src) => src,
        Err(gaps) => return gaps,
    };
    let mut gaps = Vec::new();
    for kept in [SEVERANCE, LOCAL_COLD_START] {
        if !src.contains(kept) {
            gaps.push(format!("snapshot isolation lost: {kept}"));
        }
    }
    if src.text().contains(SNAPSHOT_SEVERANCE) {
        gaps.push("scheduled snapshot re-joined channel-clearing severance".to_string());
    }
    gaps
}

/// Snapshot metadata gates the legacy channel session-key refresh.
pub(crate) fn snapshot_refresh_gaps(source: &str) -> Vec<String> {
    entry(source).map_or_else(
        |gaps| gaps,
        |src| match src.contains(GUARDED_REFRESH) {
            true => Vec::new(),
            false => vec!["snapshot must not refresh the channel session key".to_string()],
        },
    )
}

type Judge = fn(&str) -> Vec<String>;

#[test]
fn headless_guards_reject_commented_steps() {
    let cases: [(Judge, &[&str]); 7] = [
        (row_refusal_gaps, &[ROW_REFUSAL, SPAWN]),
        (post_spawn_gaps, &[SPAWN, STARTED]),
        (lifecycle_take_gaps, &[LIFECYCLE, CONSUMED, TAKE]),
        (take_once_gaps, &[TAKE, PROMPT_USES[0], PROMPT_USES[1]]),
        (headless_profile_gaps, &[BUILDER, HEADLESS_PROFILE]),
        (snapshot_severance_gaps, &[SEVERANCE, LOCAL_COLD_START]),
        (snapshot_refresh_gaps, &[GUARDED_REFRESH]),
    ];
    for (judge, needles) in cases {
        assert_eq!(judge(HEADLESS_TURN), Vec::<String>::new());
        for needle in needles.iter().chain(&[ENTRY]) {
            for forgery in CommentForgery::ALL {
                let forged = forge(HEADLESS_TURN, needle, forgery);
                assert!(forged.contains(needle));
                assert!(!judge(&forged).is_empty(), "{forgery:?} `{needle}` passes");
            }
        }
    }
}

#[test]
fn headless_guards_reject_reordered_steps_and_count_coded_bans() {
    let take_late = swap(HEADLESS_TURN, CONSUMED, TAKE);
    assert!(!lifecycle_take_gaps(&take_late).is_empty());
    let spawn_first = swap(HEADLESS_TURN, ROW_REFUSAL, SPAWN);
    assert!(!row_refusal_gaps(&spawn_first).is_empty());
    let lexed = Lexed::new(HEADLESS_TURN);
    let after_spawn = lexed
        .find(SPAWN)
        .and_then(|spawn| lexed.find_after(spawn, STARTED));
    let cases: [(Judge, Option<usize>, &str); 3] = [
        (
            post_spawn_gaps,
            after_spawn,
            "HeadlessTurnStartError::Busy;",
        ),
        (
            headless_profile_gaps,
            lexed.find(HEADLESS_PROFILE),
            "PromptProfiles::foreground(x),",
        ),
        (
            snapshot_severance_gaps,
            lexed.find(SEVERANCE),
            "let _ = goal_fresh || fresh_routine || scheduled_snapshot_turn;",
        ),
    ];
    for (judge, at, banned) in cases {
        let at = at.expect("insertion anchor");
        let insert = |text: &str| format!("{}{text}{}", &HEADLESS_TURN[..at], &HEADLESS_TURN[at..]);
        let prose = insert(&format!("/* outer /* {banned} */ */ // {banned}\n"));
        assert_eq!(judge(&prose), Vec::<String>::new(), "{banned}");
        assert_eq!(judge(&insert(&format!("{banned}\n"))).len(), 1, "{banned}");
    }
}
