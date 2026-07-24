use super::*;

pub(crate) fn claude_tui_transcript_fallback_path(
    provider: &crate::services::provider::ProviderKind,
    tmux_session_name: &str,
    workspace: Option<&str>,
    restored_cwd: Option<&str>,
    shared: &Arc<SharedData>,
    claude_home: Option<&std::path::Path>,
    restore_claimed_transcripts: &std::collections::HashSet<std::path::PathBuf>,
) -> Option<String> {
    if *provider != crate::services::provider::ProviderKind::Claude {
        return None;
    }
    let scan_context = claude_tui_restore_scan_context(tmux_session_name, restored_cwd, workspace)?;
    let mut claimed_by_other_sessions =
        super::super::super::tui_prompt_relay::other_session_claimed_transcripts(
            shared,
            tmux_session_name,
        );
    claimed_by_other_sessions.extend(restore_claimed_transcripts.iter().cloned());
    claude_tui_transcript_fallback_path_for_context(
        provider,
        &scan_context.cwd,
        scan_context.modified_since,
        claude_home,
        &claimed_by_other_sessions,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaudeTuiRestoreScanContext {
    cwd: std::path::PathBuf,
    modified_since: std::time::SystemTime,
}

fn claude_tui_restore_scan_context(
    tmux_session_name: &str,
    restored_cwd: Option<&str>,
    workspace: Option<&str>,
) -> Option<ClaudeTuiRestoreScanContext> {
    let launch_context =
        super::super::super::tui_prompt_relay::claude_tui_launch_context(tmux_session_name);
    let fallback_modified_since = if launch_context.is_none() {
        claude_tui_restore_fallback_modified_since(tmux_session_name)
    } else {
        None
    };
    select_claude_tui_restore_scan_context(
        launch_context,
        fallback_modified_since,
        restored_cwd,
        workspace,
    )
}

fn select_claude_tui_restore_scan_context(
    launch_context: Option<(std::path::PathBuf, std::time::SystemTime)>,
    fallback_modified_since: Option<std::time::SystemTime>,
    restored_cwd: Option<&str>,
    workspace: Option<&str>,
) -> Option<ClaudeTuiRestoreScanContext> {
    if let Some((launch_cwd, launch_mtime)) = launch_context {
        let cwd = select_claude_tui_restore_scan_cwd(Some(launch_cwd), restored_cwd, workspace)?;
        return Some(ClaudeTuiRestoreScanContext {
            cwd,
            modified_since: launch_mtime,
        });
    }
    let cwd = select_claude_tui_restore_scan_cwd(None, restored_cwd, workspace)?;
    Some(ClaudeTuiRestoreScanContext {
        cwd,
        modified_since: fallback_modified_since?,
    })
}

fn claude_tui_restore_fallback_modified_since(
    tmux_session_name: &str,
) -> Option<std::time::SystemTime> {
    [
        crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
        "generation",
        crate::services::tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT,
    ]
    .into_iter()
    .filter_map(|ext| {
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, ext)
    })
    .filter_map(|path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    })
    .next()
}

fn select_claude_tui_restore_scan_cwd(
    launch_cwd: Option<std::path::PathBuf>,
    restored_cwd: Option<&str>,
    workspace: Option<&str>,
) -> Option<std::path::PathBuf> {
    launch_cwd
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| {
            restored_cwd
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(std::path::PathBuf::from)
        })
        .or_else(|| {
            workspace
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(std::path::PathBuf::from)
        })
}

fn claude_tui_transcript_fallback_path_for_context(
    provider: &crate::services::provider::ProviderKind,
    cwd: &std::path::Path,
    launch_mtime: std::time::SystemTime,
    claude_home: Option<&std::path::Path>,
    exclude: &std::collections::HashSet<std::path::PathBuf>,
) -> Option<String> {
    if *provider != crate::services::provider::ProviderKind::Claude {
        return None;
    }
    let transcript =
        crate::services::claude_tui::transcript_tail::latest_claude_transcript_for_cwd(
            cwd,
            launch_mtime,
            claude_home,
            exclude,
        )?;
    Some(transcript.display().to_string())
}

#[cfg(test)]
mod claude_tui_transcript_fallback_tests {
    use crate::services::provider::ProviderKind;
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    fn write_transcript(home: &Path, cwd: &Path, session_id: &str, body: &[u8]) -> PathBuf {
        let transcript = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd,
            session_id,
            Some(home),
        )
        .unwrap();
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, body).unwrap();
        transcript
    }

    #[test]
    fn resolves_freshest_claude_transcript_when_wrapper_jsonl_absent() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let session_id = "11111111-1111-4111-8111-111111111111";
        let transcript = write_transcript(
            home.path(),
            cwd.path(),
            session_id,
            b"{\"type\":\"assistant\"}\n",
        );

        let resolved = super::claude_tui_transcript_fallback_path_for_context(
            &ProviderKind::Claude,
            cwd.path(),
            SystemTime::UNIX_EPOCH,
            Some(home.path()),
            &HashSet::new(),
        );
        assert_eq!(
            resolved.as_deref(),
            transcript.to_str(),
            "claude_tui fallback must recover onto the live rollout transcript"
        );
    }

    #[test]
    fn returns_none_for_non_claude_provider() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        assert!(
            super::claude_tui_transcript_fallback_path_for_context(
                &ProviderKind::Codex,
                cwd.path(),
                SystemTime::UNIX_EPOCH,
                Some(home.path()),
                &HashSet::new(),
            )
            .is_none(),
            "codex uses its own rollout fallback, not the claude transcript path"
        );
    }

    #[test]
    fn returns_none_without_transcript() {
        let home = tempfile::tempdir().unwrap();
        let empty_cwd = tempfile::tempdir().unwrap();
        assert!(
            super::claude_tui_transcript_fallback_path_for_context(
                &ProviderKind::Claude,
                empty_cwd.path(),
                SystemTime::UNIX_EPOCH,
                Some(home.path()),
                &HashSet::new(),
            )
            .is_none()
        );
    }

    #[test]
    fn excludes_transcripts_claimed_by_other_shared_workspace_sessions() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let own_transcript = write_transcript(
            home.path(),
            cwd.path(),
            "22222222-2222-4222-8222-222222222222",
            b"{\"type\":\"assistant\",\"session\":\"own\"}\n",
        );
        std::thread::sleep(Duration::from_millis(20));
        let other_transcript = write_transcript(
            home.path(),
            cwd.path(),
            "33333333-3333-4333-8333-333333333333",
            b"{\"type\":\"assistant\",\"session\":\"other\"}\n",
        );
        let exclude = HashSet::from([other_transcript]);

        let resolved = super::claude_tui_transcript_fallback_path_for_context(
            &ProviderKind::Claude,
            cwd.path(),
            SystemTime::UNIX_EPOCH,
            Some(home.path()),
            &exclude,
        );

        assert_eq!(
            resolved.as_deref(),
            own_transcript.to_str(),
            "shared-workspace restore must not steal another live session transcript"
        );
    }

    #[test]
    fn applies_launch_time_floor_to_skip_prior_session_transcripts() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        write_transcript(
            home.path(),
            cwd.path(),
            "44444444-4444-4444-8444-444444444444",
            b"{\"type\":\"assistant\",\"session\":\"old\"}\n",
        );
        std::thread::sleep(Duration::from_millis(20));
        let launch_mtime = SystemTime::now();
        std::thread::sleep(Duration::from_millis(20));
        let current_transcript = write_transcript(
            home.path(),
            cwd.path(),
            "55555555-5555-4555-8555-555555555555",
            b"{\"type\":\"assistant\",\"session\":\"current\"}\n",
        );

        let resolved = super::claude_tui_transcript_fallback_path_for_context(
            &ProviderKind::Claude,
            cwd.path(),
            launch_mtime,
            Some(home.path()),
            &HashSet::new(),
        );

        assert_eq!(
            resolved.as_deref(),
            current_transcript.to_str(),
            "restart restore must ignore transcripts older than the tmux launch"
        );
    }

    #[test]
    fn restore_allocation_excludes_already_selected_rotated_transcript_for_same_cwd_sessions() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let first_rotated = write_transcript(
            home.path(),
            cwd.path(),
            "66666666-6666-4666-8666-666666666666",
            b"{\"type\":\"assistant\",\"session\":\"rotated-a\"}\n",
        );
        std::thread::sleep(Duration::from_millis(20));
        let second_rotated = write_transcript(
            home.path(),
            cwd.path(),
            "77777777-7777-4777-8777-777777777777",
            b"{\"type\":\"assistant\",\"session\":\"rotated-b\"}\n",
        );
        let launch_mtime = SystemTime::UNIX_EPOCH;
        let mut restore_claims = HashSet::new();

        let first = super::claude_tui_transcript_fallback_path_for_context(
            &ProviderKind::Claude,
            cwd.path(),
            launch_mtime,
            Some(home.path()),
            &restore_claims,
        )
        .expect("first restore selection");
        restore_claims.insert(PathBuf::from(&first));
        let second = super::claude_tui_transcript_fallback_path_for_context(
            &ProviderKind::Claude,
            cwd.path(),
            launch_mtime,
            Some(home.path()),
            &restore_claims,
        )
        .expect("second restore selection");

        assert_eq!(first, second_rotated.to_string_lossy());
        assert_eq!(second, first_rotated.to_string_lossy());
        assert_ne!(
            first, second,
            "same restore scan must allocate distinct rotated transcripts"
        );
    }

    #[test]
    fn missing_launch_context_uses_restored_cwd_with_marker_mtime_floor() {
        let home = tempfile::tempdir().unwrap();
        let restored_cwd = tempfile::tempdir().unwrap();
        let configured_cwd = tempfile::tempdir().unwrap();
        write_transcript(
            home.path(),
            restored_cwd.path(),
            "88888888-8888-4888-8888-888888888888",
            b"{\"type\":\"assistant\",\"session\":\"old\"}\n",
        );
        std::thread::sleep(Duration::from_millis(20));
        let marker_mtime = SystemTime::now();
        std::thread::sleep(Duration::from_millis(20));
        let current_transcript = write_transcript(
            home.path(),
            restored_cwd.path(),
            "99999999-9999-4999-8999-999999999999",
            b"{\"type\":\"assistant\",\"session\":\"current\"}\n",
        );
        let configured_transcript = write_transcript(
            home.path(),
            configured_cwd.path(),
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            b"{\"type\":\"assistant\",\"session\":\"configured\"}\n",
        );

        let context = super::select_claude_tui_restore_scan_context(
            None,
            Some(marker_mtime),
            Some(restored_cwd.path().to_str().unwrap()),
            Some(configured_cwd.path().to_str().unwrap()),
        )
        .expect("missing launch script should still scan with marker floor");

        assert_eq!(context.cwd, restored_cwd.path());
        assert_eq!(context.modified_since, marker_mtime);
        let resolved = super::claude_tui_transcript_fallback_path_for_context(
            &ProviderKind::Claude,
            &context.cwd,
            context.modified_since,
            Some(home.path()),
            &HashSet::new(),
        );

        assert_eq!(resolved.as_deref(), current_transcript.to_str());
        assert_ne!(resolved.as_deref(), configured_transcript.to_str());
    }

    #[test]
    fn restore_scan_cwd_prefers_actual_launch_worktree_then_db_then_configured_workspace() {
        let configured = tempfile::tempdir().unwrap();
        let db_worktree = tempfile::tempdir().unwrap();
        let launch_worktree = tempfile::tempdir().unwrap();

        assert_eq!(
            super::select_claude_tui_restore_scan_cwd(
                Some(launch_worktree.path().to_path_buf()),
                Some(db_worktree.path().to_str().unwrap()),
                Some(configured.path().to_str().unwrap()),
            )
            .as_deref(),
            Some(launch_worktree.path())
        );
        assert_eq!(
            super::select_claude_tui_restore_scan_cwd(
                None,
                Some(db_worktree.path().to_str().unwrap()),
                Some(configured.path().to_str().unwrap()),
            )
            .as_deref(),
            Some(db_worktree.path())
        );
        assert_eq!(
            super::select_claude_tui_restore_scan_cwd(
                None,
                Some("   "),
                Some(configured.path().to_str().unwrap()),
            )
            .as_deref(),
            Some(configured.path())
        );
    }
}
