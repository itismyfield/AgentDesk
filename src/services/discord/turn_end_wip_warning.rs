//! Turn-end WIP warning completion-surface merge (#3792, #4217).
//!
//! This module bridges the generic git detector in `utils::wip_detect` to the
//! Discord completion helpers. A per-inflight reservation coordinates duplicate
//! warning suppression across the bridge, watcher, and footer reconciler.

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use poise::serenity_prelude::ChannelId;

use super::inflight::InflightTurnState;
use crate::services::provider::ProviderKind;
use crate::utils::wip_detect::{WipWarning, check_wip_uncommitted_files};

const WIP_WARNING_MARKER: &str = "⚠️ **턴을 완료하기 전에 커밋되지 않은 변경사항을 확인하세요.**";
const MAX_RECORDED_DELIVERIES: usize = 2_048;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TurnEndWipKey {
    provider: String,
    channel_id: u64,
    user_msg_id: u64,
}

#[derive(Default)]
struct TurnEndWipDedup {
    claimed: HashSet<TurnEndWipKey>,
    delivered: HashSet<TurnEndWipKey>,
    delivery_order: VecDeque<TurnEndWipKey>,
}

static TURN_END_WIP_DEDUP: LazyLock<Mutex<TurnEndWipDedup>> =
    LazyLock::new(|| Mutex::new(TurnEndWipDedup::default()));

pub(in crate::services::discord) struct TurnEndWipWarningReservation {
    key: TurnEndWipKey,
    text: String,
    committed: bool,
}

impl TurnEndWipWarningReservation {
    pub(in crate::services::discord) fn text(&self) -> &str {
        &self.text
    }

    pub(in crate::services::discord) fn commit(mut self) {
        let mut dedup = TURN_END_WIP_DEDUP
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        dedup.claimed.remove(&self.key);
        if dedup.delivered.insert(self.key.clone()) {
            dedup.delivery_order.push_back(self.key.clone());
        }
        while dedup.delivery_order.len() > MAX_RECORDED_DELIVERIES {
            if let Some(expired) = dedup.delivery_order.pop_front() {
                dedup.delivered.remove(&expired);
            }
        }
        self.committed = true;
    }
}

impl Drop for TurnEndWipWarningReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        TURN_END_WIP_DEDUP
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .claimed
            .remove(&self.key);
    }
}

pub(in crate::services::discord) fn load_matching_inflight_state(
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_msg_id: Option<u64>,
) -> Option<InflightTurnState> {
    let expected_user_msg_id = expected_user_msg_id?;
    if expected_user_msg_id == 0 {
        return None;
    }
    let state = super::inflight::load_inflight_state(provider, channel_id.get())?;
    if state.user_msg_id == expected_user_msg_id {
        Some(state)
    } else {
        None
    }
}

pub(in crate::services::discord) fn turn_end_wip_warning_text(
    inflight: Option<&InflightTurnState>,
) -> Option<String> {
    let worktree_path = inflight?.worktree_path.as_deref()?.trim();
    if worktree_path.is_empty() {
        return None;
    }
    let workspace = Path::new(worktree_path);
    if !workspace.is_dir() {
        return None;
    }
    let warning = check_wip_uncommitted_files(workspace)?;
    Some(format_turn_end_wip_warning(&warning))
}

fn format_turn_end_wip_warning(warning: &WipWarning) -> String {
    format!(
        "{WIP_WARNING_MARKER}\n\
         작업공간: `{}`\n\
         파일 수: 스테이징됨 {}개 · 스테이징 안 됨 {}개 · 추적되지 않음 {}개\n\
         턴을 끝내기 전에 변경사항을 커밋하거나 명시적으로 폐기하세요.",
        warning.workspace.display().to_string().replace('`', "\\`"),
        warning.staged.len(),
        warning.unstaged.len(),
        warning.untracked.len()
    )
}

pub(in crate::services::discord) fn reserve_turn_end_wip_warning(
    inflight: Option<&InflightTurnState>,
) -> Option<TurnEndWipWarningReservation> {
    let inflight = inflight?;
    if inflight.user_msg_id == 0 {
        return None;
    }
    let text = turn_end_wip_warning_text(Some(inflight))?;
    let key = TurnEndWipKey {
        provider: inflight.provider.clone(),
        channel_id: inflight.channel_id,
        user_msg_id: inflight.user_msg_id,
    };
    let mut dedup = TURN_END_WIP_DEDUP
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if dedup.claimed.contains(&key) || dedup.delivered.contains(&key) {
        return None;
    }
    dedup.claimed.insert(key.clone());
    Some(TurnEndWipWarningReservation {
        key,
        text,
        committed: false,
    })
}

pub(in crate::services::discord) fn turn_end_wip_warning_was_delivered(
    inflight: Option<&InflightTurnState>,
) -> bool {
    let Some(inflight) = inflight.filter(|state| state.user_msg_id != 0) else {
        return false;
    };
    let key = TurnEndWipKey {
        provider: inflight.provider.clone(),
        channel_id: inflight.channel_id,
        user_msg_id: inflight.user_msg_id,
    };
    TURN_END_WIP_DEDUP
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .delivered
        .contains(&key)
}

pub(in crate::services::discord) fn merge_turn_end_wip_warning(
    completion_surface: String,
    reservation: Option<&TurnEndWipWarningReservation>,
) -> String {
    let Some(warning) = reservation.map(TurnEndWipWarningReservation::text) else {
        return completion_surface;
    };
    merge_bounded_turn_end_wip_warning(completion_surface, warning)
}

/// Appends the WIP warning within the Discord bound, trimming completion first.
///
/// Warning-only overflow keeps an unmarked prefix and may lose inline-backtick closure or details.
/// Escaping makes each backtick byte two UTF-16 units, so existing Darwin/Linux paths can reach it; the result stays bounded.
pub(in crate::services::discord) fn merge_bounded_turn_end_wip_warning(
    completion_surface: String,
    warning: &str,
) -> String {
    use super::formatting::{byte_index_at_discord_message_units, discord_message_units};

    let completion_surface = completion_surface.trim_end();
    let warning = warning.trim_end();
    if warning.is_empty() {
        return completion_surface.to_string();
    }
    let merged = if completion_surface.is_empty() {
        warning.to_string()
    } else {
        format!("{completion_surface}\n\n{warning}")
    };
    if discord_message_units(&merged) <= super::DISCORD_MSG_LIMIT {
        return merged;
    }

    let warning = bounded_turn_end_wip_warning(warning, super::DISCORD_MSG_LIMIT);
    let separator = "\n\n";
    let body_budget = super::DISCORD_MSG_LIMIT.saturating_sub(
        discord_message_units(&warning).saturating_add(discord_message_units(separator)),
    );
    let body_end = byte_index_at_discord_message_units(completion_surface, body_budget);
    let completion_surface =
        super::single_message_panel::repair_fence_parity(&completion_surface[..body_end]);
    let completion_surface = completion_surface.trim_end();
    if completion_surface.is_empty() {
        warning
    } else {
        format!("{completion_surface}{separator}{warning}")
    }
}

/// Longest `max_units` prefix; repairs only triple fences, adds no marker, and may lose inline closure/details.
pub(in crate::services::discord) fn bounded_turn_end_wip_warning(
    warning: &str,
    max_units: usize,
) -> String {
    let warning = warning.trim_end();
    let end = super::formatting::byte_index_at_discord_message_units(warning, max_units);
    super::single_message_panel::repair_fence_parity(&warning[..end])
}

/// Separates warning for final-wire budgeting; post-warning metadata moves before it, reversing prior wire order.
pub(in crate::services::discord) fn split_merged_turn_end_wip_warning(
    completion_block: &str,
) -> Option<(String, String)> {
    const METADATA_BOUNDARY: &str = "\n\n\u{2063}\n";

    let (completion_block, warning_tail) = completion_block.split_once(WIP_WARNING_MARKER)?;
    let (warning_tail, metadata_suffix) = warning_tail
        .rfind(METADATA_BOUNDARY)
        .map(|index| warning_tail.split_at(index))
        .unwrap_or((warning_tail, ""));
    let mut completion_without_warning = completion_block.trim_end().to_string();
    completion_without_warning.push_str(metadata_suffix);
    Some((
        completion_without_warning,
        format!("{WIP_WARNING_MARKER}{}", warning_tail.trim_end()),
    ))
}

pub(in crate::services::discord) fn preserve_merged_turn_end_wip_warning(
    completion_surface: String,
    previous_surface: &str,
) -> String {
    let Some((_, warning)) = previous_surface.split_once(WIP_WARNING_MARKER) else {
        return completion_surface;
    };
    merge_bounded_turn_end_wip_warning(
        completion_surface,
        &format!("{WIP_WARNING_MARKER}{warning}"),
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    use super::*;
    use crate::services::git::GitCommand;

    fn warning_for_workspace(workspace: std::path::PathBuf) -> String {
        format_turn_end_wip_warning(&WipWarning {
            workspace,
            staged: vec!["staged.txt".to_string()],
            unstaged: Vec::new(),
            untracked: Vec::new(),
        })
    }

    #[test]
    fn final_wire_reserves_normal_warning_after_full_panel_and_metadata_5325() {
        let warning = warning_for_workspace("/tmp/agentdesk-worktree".into());
        let panel = "x".repeat(crate::services::discord::DISCORD_MSG_LIMIT);
        let merged = merge_bounded_turn_end_wip_warning(panel, &warning);
        assert_eq!(
            crate::services::discord::formatting::discord_message_units(&merged),
            crate::services::discord::DISCORD_MSG_LIMIT
        );
        assert!(merged.ends_with(&warning));
        let completion_block = format!("{merged}\n\n\u{2063}\n⏱ 1s");
        let wire = crate::services::discord::single_message_panel::compose_completion_footer_text(
            "terminal body",
            Some(&completion_block),
        );
        let rendered_warning =
            crate::services::discord::single_message_panel::completion_footer_subtext(&warning);

        assert!(wire.ends_with(&rendered_warning));
        let ordered_completion = format!("panel\n\n{warning}\n\n\u{2063}\n⏱ 1s");
        let ordered_wire =
            crate::services::discord::single_message_panel::compose_completion_footer_text(
                "terminal body",
                Some(&ordered_completion),
            );
        let metadata_position = ordered_wire
            .find("-# ⏱ 1s")
            .expect("in-budget metadata must survive");
        let warning_position = ordered_wire
            .find(&rendered_warning)
            .expect("reserved warning must survive");

        assert!(metadata_position < warning_position);
        assert!(
            crate::services::discord::formatting::discord_message_units(&wire)
                <= crate::services::discord::DISCORD_MSG_LIMIT
        );
        assert!(crate::services::discord::http::discord_content_or_zwsp(&wire).is_ok());
    }

    #[test]
    fn final_wire_bounds_oversized_worktree_warning_5325() {
        let mut workspace = std::path::PathBuf::from("/worktrees");
        for _ in 0..9 {
            workspace.push("w".repeat(240));
        }
        let warning = warning_for_workspace(workspace);
        assert!(
            crate::services::discord::formatting::discord_message_units(&warning)
                > crate::services::discord::DISCORD_MSG_LIMIT
        );

        let merged = merge_bounded_turn_end_wip_warning("panel".to_string(), &warning);
        let wire = crate::services::discord::single_message_panel::compose_completion_footer_text(
            "terminal body",
            Some(&merged),
        );

        assert!(wire.contains(WIP_WARNING_MARKER));
        assert!(wire.contains("작업공간: `"));
        assert_eq!(wire.matches('`').count() % 2, 1);
        assert!(!wire.contains("파일 수:"));
        assert!(!wire.contains("커밋하거나 명시적으로 폐기하세요"));
        assert!(!wire.contains("..."));
        assert!(
            crate::services::discord::formatting::discord_message_units(&wire)
                <= crate::services::discord::DISCORD_MSG_LIMIT
        );
        assert!(crate::services::discord::http::discord_content_or_zwsp(&wire).is_ok());
    }

    struct RuntimeRootGuard {
        previous: Option<std::ffi::OsString>,
        _root: TempDir,
    }

    impl RuntimeRootGuard {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("runtime root tempdir");
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
            Self {
                previous,
                _root: root,
            }
        }
    }

    impl Drop for RuntimeRootGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn git_available() -> bool {
        GitCommand::new().arg("--version").run_output().is_ok()
    }

    fn init_git_repo() -> Option<TempDir> {
        if !git_available() {
            return None;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        GitCommand::new()
            .repo(temp.path())
            .arg("init")
            .run_output()
            .expect("git init");
        Some(temp)
    }

    fn git(repo: &Path, args: &[&str]) {
        GitCommand::new()
            .repo(repo)
            .args(args.iter().copied())
            .run_output()
            .unwrap_or_else(|error| panic!("git {args:?} failed: {error}"));
    }

    fn committed_repo() -> Option<TempDir> {
        let temp = init_git_repo()?;
        fs::write(temp.path().join("tracked.txt"), "base\n").expect("seed tracked file");
        git(temp.path(), &["add", "tracked.txt"]);
        git(
            temp.path(),
            &[
                "-c",
                "user.name=AgentDesk Test",
                "-c",
                "user.email=agentdesk-test@example.invalid",
                "commit",
                "-m",
                "initial",
            ],
        );
        Some(temp)
    }

    fn inflight_for_worktree(worktree_path: &Path) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            42,
            Some("wip-warning-test".to_string()),
            1,
            2,
            3,
            "test".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        state.worktree_path = Some(worktree_path.display().to_string());
        state
    }

    #[test]
    fn dirty_warning_text_includes_path_and_counts() {
        let Some(temp) = committed_repo() else {
            return;
        };
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let _root = RuntimeRootGuard::new();
        fs::write(temp.path().join("tracked.txt"), "modified\n").expect("modify tracked file");
        fs::write(temp.path().join("staged.txt"), "staged\n").expect("write staged file");
        git(temp.path(), &["add", "staged.txt"]);
        fs::write(temp.path().join("untracked.txt"), "untracked\n").expect("write untracked");

        let state = inflight_for_worktree(temp.path());
        let text = turn_end_wip_warning_text(Some(&state)).expect("dirty warning text");

        assert!(text.contains("커밋되지 않은 변경사항"));
        assert!(text.contains(&format!("작업공간: `{}`", temp.path().display())));
        assert!(text.contains("파일 수: 스테이징됨 1개 · 스테이징 안 됨 1개 · 추적되지 않음 1개"));
        assert!(!text.contains("WARNING:"));
    }

    #[test]
    fn zero_expected_user_msg_id_never_matches_inflight() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let _root = RuntimeRootGuard::new();
        let temp = tempfile::tempdir().expect("worktree tempdir");
        let mut state = inflight_for_worktree(temp.path());
        state.user_msg_id = 0;
        crate::services::discord::inflight::save_inflight_state(&state)
            .expect("save zero-owned inflight state");

        assert!(
            load_matching_inflight_state(&ProviderKind::Claude, ChannelId::new(42), Some(0))
                .is_none()
        );
    }

    #[test]
    fn clean_non_git_missing_and_missing_workspace_suppress_warning() {
        let Some(clean) = committed_repo() else {
            return;
        };
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let _root = RuntimeRootGuard::new();
        let clean_state = inflight_for_worktree(clean.path());
        assert_eq!(turn_end_wip_warning_text(Some(&clean_state)), None);

        let non_git = tempfile::tempdir().expect("non git tempdir");
        let non_git_state = inflight_for_worktree(non_git.path());
        assert_eq!(turn_end_wip_warning_text(Some(&non_git_state)), None);

        let missing_path = non_git.path().join("missing");
        let missing_state = inflight_for_worktree(&missing_path);
        assert_eq!(turn_end_wip_warning_text(Some(&missing_state)), None);

        assert_eq!(turn_end_wip_warning_text(None), None);
    }

    #[test]
    fn dropped_reservation_can_be_claimed_by_another_completion_path() {
        let Some(temp) = init_git_repo() else {
            return;
        };
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let _root = RuntimeRootGuard::new();
        fs::write(temp.path().join("untracked.txt"), "untracked\n").expect("write untracked");
        let state = inflight_for_worktree(temp.path());

        let first = reserve_turn_end_wip_warning(Some(&state)).expect("first reservation");
        assert!(reserve_turn_end_wip_warning(Some(&state)).is_none());
        drop(first);
        assert!(reserve_turn_end_wip_warning(Some(&state)).is_some());
    }

    #[test]
    fn committed_reservation_suppresses_other_completion_paths() {
        let Some(temp) = init_git_repo() else {
            return;
        };
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let _root = RuntimeRootGuard::new();
        fs::write(temp.path().join("untracked.txt"), "untracked\n").expect("write untracked");
        let mut state = inflight_for_worktree(temp.path());
        state.user_msg_id = 33_792_001;

        reserve_turn_end_wip_warning(Some(&state))
            .expect("first completion path reservation")
            .commit();
        assert!(reserve_turn_end_wip_warning(Some(&state)).is_none());
    }
}
