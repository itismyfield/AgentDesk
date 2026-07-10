use std::path::{Path, PathBuf};

use super::rebind_runtime::RebindRuntimeState;
use super::*;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ManualRebindOverrides {
    output_path: Option<String>,
    session_id: Option<String>,
}

impl ManualRebindOverrides {
    pub(crate) fn validated(
        provider: &ProviderKind,
        output_path: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Self, String> {
        let output_path = output_path
            .map(str::trim)
            .map(|path| validate_output_path(provider, path))
            .transpose()?;
        let session_id = session_id
            .map(str::trim)
            .map(validate_session_id)
            .transpose()?;
        validate_claude_override_coherence(
            provider,
            output_path.as_deref(),
            session_id.as_deref(),
        )?;
        Ok(Self {
            output_path,
            session_id,
        })
    }

    pub(crate) fn output_path(&self) -> Option<&str> {
        self.output_path.as_deref()
    }

    pub(crate) fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub(super) fn runtime_state(
        &self,
        provider: &ProviderKind,
        tmux_session_name: &str,
        fallback_session_id: Option<String>,
    ) -> Result<Option<RebindRuntimeState>, RebindError> {
        let Some(output_path) = self.output_path.as_ref() else {
            return Ok(None);
        };
        let output_len = std::fs::metadata(output_path)
            .map_err(|error| {
                RebindError::Internal(format!(
                    "stat validated output_path override {output_path}: {error}"
                ))
            })?
            .len();
        let runtime_binding =
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name);
        let runtime_kind =
            crate::services::tmux_common::resolve_tmux_runtime_kind_marker(tmux_session_name)
                .or_else(|| runtime_binding.as_ref().map(|binding| binding.runtime_kind))
                .or_else(|| {
                    (provider == &ProviderKind::Claude).then_some(RuntimeHandoffKind::ClaudeTui)
                });
        let session_id = self
            .session_id
            .clone()
            .or_else(|| claude_session_id_from_path(provider, output_path))
            .or(fallback_session_id);
        let input_fifo_path = if matches!(
            runtime_kind,
            Some(RuntimeHandoffKind::ClaudeTui | RuntimeHandoffKind::CodexTui)
        ) {
            None
        } else {
            Some(tmux_runtime_paths(tmux_session_name).1)
        };
        let codex_rollout_path = (provider == &ProviderKind::Codex
            && runtime_kind == Some(RuntimeHandoffKind::CodexTui))
        .then(|| output_path.clone());

        Ok(Some(RebindRuntimeState {
            output_path: output_path.clone(),
            synthetic_initial_offset: output_len,
            input_fifo_path,
            runtime_kind,
            session_id,
            codex_rollout_path,
            codex_rollout_resume_offset: None,
            codex_rollout_resume_offset_from_marker: false,
            force_initial_offset: Some(output_len),
            rebase_existing_offsets_to_output: true,
        }))
    }
}

fn validate_session_id(session_id: &str) -> Result<String, String> {
    if session_id.is_empty() || uuid::Uuid::parse_str(session_id).is_err() {
        return Err("session_id override must be a UUID".to_string());
    }
    Ok(session_id.to_string())
}

fn validate_output_path(provider: &ProviderKind, output_path: &str) -> Result<String, String> {
    if output_path.is_empty() {
        return Err("output_path override must not be empty".to_string());
    }
    let canonical = std::fs::canonicalize(output_path)
        .map_err(|_| "output_path override must exist".to_string())?;
    let metadata =
        std::fs::metadata(&canonical).map_err(|_| "output_path override must exist".to_string())?;
    if !metadata.is_file() {
        return Err("output_path override must be a regular file".to_string());
    }
    if !is_under_allowed_output_root(provider, &canonical) {
        let error = match provider {
            ProviderKind::Claude => {
                "Claude output_path override must be under a Claude projects directory".to_string()
            }
            _ => format!(
                "{} output_path override must be under an allowed session directory",
                provider.as_str()
            ),
        };
        return Err(error);
    }
    Ok(canonical.display().to_string())
}

fn is_under_allowed_output_root(provider: &ProviderKind, path: &Path) -> bool {
    allowed_output_roots(provider)
        .into_iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .any(|root| path.starts_with(root))
}

fn allowed_output_roots(provider: &ProviderKind) -> Vec<PathBuf> {
    match provider {
        ProviderKind::Claude => claude_projects_dir_candidates(),
        ProviderKind::Codex => {
            crate::services::codex_tui::rollout_tail::default_codex_sessions_dir()
                .into_iter()
                .chain(crate::services::tmux_common::persistent_sessions_dir())
                .collect()
        }
        ProviderKind::Gemini | ProviderKind::OpenCode | ProviderKind::Qwen => {
            crate::services::tmux_common::persistent_sessions_dir()
                .into_iter()
                .collect()
        }
        ProviderKind::Unsupported(_) => Vec::new(),
    }
}

fn claude_projects_dir_candidates() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(config_dir) =
        std::env::var_os("CLAUDE_CONFIG_DIR").filter(|value| !value.is_empty())
    {
        roots.push(PathBuf::from(config_dir).join("projects"));
    }
    if let Some(home) = dirs::home_dir() {
        let default = home.join(".claude/projects");
        if !roots.contains(&default) {
            roots.push(default);
        }
    }
    roots
}

fn claude_session_id_from_path(provider: &ProviderKind, output_path: &str) -> Option<String> {
    if provider != &ProviderKind::Claude {
        return None;
    }
    let stem = Path::new(output_path).file_stem()?.to_str()?;
    uuid::Uuid::parse_str(stem).ok().map(|_| stem.to_string())
}

fn validate_claude_override_coherence(
    provider: &ProviderKind,
    output_path: Option<&str>,
    session_id: Option<&str>,
) -> Result<(), String> {
    if provider != &ProviderKind::Claude {
        return Ok(());
    }
    let (Some(output_path), Some(session_id)) = (output_path, session_id) else {
        return Ok(());
    };
    let path_session_id = Path::new(output_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| uuid::Uuid::parse_str(stem).ok());
    let supplied_session_id = uuid::Uuid::parse_str(session_id)
        .expect("validated session_id override must remain a UUID");
    if path_session_id != Some(supplied_session_id) {
        return Err(
            "Claude output_path transcript UUID must match session_id override".to_string(),
        );
    }
    Ok(())
}

pub(super) async fn upsert_rebind_session_id_override(
    shared: &SharedData,
    provider: &ProviderKind,
    tmux_session_name: &str,
    session_id: Option<&str>,
) -> Result<(), RebindError> {
    let Some(session_id) = session_id else {
        return Ok(());
    };
    let Some(pool) = shared.pg_pool.as_ref() else {
        return Ok(());
    };
    let candidates = super::super::adk_session::build_session_key_candidates(
        &shared.token_hash,
        provider,
        tmux_session_name,
    );
    let mut selected = candidates[0].as_str();
    for candidate in &candidates {
        match crate::db::dispatched_sessions::load_provider_session_ids_pg(
            pool,
            candidate,
            Some(provider.as_str()),
        )
        .await
        {
            Ok(Some(_)) => {
                selected = candidate;
                break;
            }
            Ok(None) => {}
            Err(error) => return Err(RebindError::Internal(error)),
        }
    }
    crate::db::dispatched_session_rebind_override::upsert_rebind_session_override_pg(
        pool,
        selected,
        provider.as_str(),
        session_id,
    )
    .await
    .map_err(RebindError::Internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_rebind_override_rejects_missing_and_non_regular_paths() {
        let root = tempfile::tempdir().expect("override root");
        assert!(
            ManualRebindOverrides::validated(
                &ProviderKind::Codex,
                Some(
                    root.path()
                        .join("missing.jsonl")
                        .to_str()
                        .expect("utf8 path")
                ),
                None,
            )
            .unwrap_err()
            .contains("must exist")
        );
        assert!(
            ManualRebindOverrides::validated(
                &ProviderKind::Codex,
                Some(root.path().to_str().expect("utf8 path")),
                None,
            )
            .unwrap_err()
            .contains("regular file")
        );
    }

    #[test]
    fn health_rebind_override_rejects_claude_path_outside_projects() {
        let root = tempfile::tempdir().expect("Claude home");
        let _env = crate::config::TestEnvVarGuard::set_path("CLAUDE_CONFIG_DIR", root.path());
        let outside = root.path().join("outside.jsonl");
        std::fs::write(&outside, b"{}\n").expect("outside transcript");

        let error = ManualRebindOverrides::validated(
            &ProviderKind::Claude,
            Some(outside.to_str().expect("utf8 path")),
            None,
        )
        .unwrap_err();
        assert!(error.contains("Claude projects directory"));
    }

    #[test]
    fn health_rebind_override_rejects_non_uuid_session_id() {
        assert_eq!(
            ManualRebindOverrides::validated(
                &ProviderKind::Claude,
                None,
                Some("not-a-session-uuid"),
            )
            .unwrap_err(),
            "session_id override must be a UUID"
        );
    }

    #[test]
    fn health_rebind_override_accepts_parsed_equal_claude_path_and_session_id() {
        let root = tempfile::tempdir().expect("Claude home");
        let _env = crate::config::TestEnvVarGuard::set_path("CLAUDE_CONFIG_DIR", root.path());
        let project = root.path().join("projects/-tmp-agentdesk");
        std::fs::create_dir_all(&project).expect("project dir");
        let transcript = project.join("4c474e5d-37e7-4b6a-bcf7-d68854a31c49.jsonl");
        std::fs::write(&transcript, b"{}\n").expect("transcript");

        let overrides = ManualRebindOverrides::validated(
            &ProviderKind::Claude,
            Some(transcript.to_str().expect("utf8 path")),
            Some("4C474E5D-37E7-4B6A-BCF7-D68854A31C49"),
        )
        .expect("valid override");
        assert_eq!(
            overrides.output_path(),
            Some(
                std::fs::canonicalize(transcript)
                    .expect("canonical transcript")
                    .to_str()
                    .expect("utf8 path")
            )
        );
        assert_eq!(
            overrides.session_id(),
            Some("4C474E5D-37E7-4B6A-BCF7-D68854A31C49")
        );
    }

    #[test]
    fn health_rebind_override_preserves_claude_path_only_and_session_only_forms() {
        let root = tempfile::tempdir().expect("Claude home");
        let _env = crate::config::TestEnvVarGuard::set_path("CLAUDE_CONFIG_DIR", root.path());
        let project = root.path().join("projects/-tmp-agentdesk");
        std::fs::create_dir_all(&project).expect("project dir");
        let transcript = project.join("4c474e5d-37e7-4b6a-bcf7-d68854a31c49.jsonl");
        std::fs::write(&transcript, b"{}\n").expect("transcript");

        let path_only = ManualRebindOverrides::validated(
            &ProviderKind::Claude,
            Some(transcript.to_str().expect("utf8 path")),
            None,
        )
        .expect("path-only Claude override");
        assert!(path_only.output_path().is_some());
        assert!(path_only.session_id().is_none());

        let session_only = ManualRebindOverrides::validated(
            &ProviderKind::Claude,
            None,
            Some("5d585f6e-48e8-497c-94bc-16e9369e32c6"),
        )
        .expect("session-only Claude override");
        assert!(session_only.output_path().is_none());
        assert_eq!(
            session_only.session_id(),
            Some("5d585f6e-48e8-497c-94bc-16e9369e32c6")
        );
    }

    #[test]
    fn health_rebind_override_rejects_mismatched_claude_path_and_session_id() {
        let root = tempfile::tempdir().expect("Claude home");
        let _env = crate::config::TestEnvVarGuard::set_path("CLAUDE_CONFIG_DIR", root.path());
        let project = root.path().join("projects/-tmp-agentdesk");
        std::fs::create_dir_all(&project).expect("project dir");
        let transcript = project.join("4c474e5d-37e7-4b6a-bcf7-d68854a31c49.jsonl");
        std::fs::write(&transcript, b"{}\n").expect("transcript");

        let error = ManualRebindOverrides::validated(
            &ProviderKind::Claude,
            Some(transcript.to_str().expect("utf8 path")),
            Some("5d585f6e-48e8-497c-94bc-16e9369e32c6"),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "Claude output_path transcript UUID must match session_id override"
        );
    }

    #[test]
    fn health_rebind_override_accepts_codex_rollout_under_canonical_sessions_dir() {
        let root = tempfile::tempdir().expect("Codex home");
        let _env = crate::config::TestEnvVarGuard::set_path("CODEX_HOME", root.path());
        let sessions = root.path().join("sessions/2026/07/11");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        let rollout = sessions.join("rollout-valid.jsonl");
        std::fs::write(&rollout, b"{}\n").expect("rollout");

        let overrides = ManualRebindOverrides::validated(
            &ProviderKind::Codex,
            Some(rollout.to_str().expect("utf8 path")),
            None,
        )
        .expect("in-root Codex rollout");
        assert_eq!(
            overrides.output_path(),
            Some(
                std::fs::canonicalize(rollout)
                    .expect("canonical rollout")
                    .to_str()
                    .expect("utf8 path")
            )
        );
    }

    #[test]
    fn health_rebind_override_rejects_codex_rollout_outside_canonical_sessions_dir() {
        let root = tempfile::tempdir().expect("Codex home");
        let _env = crate::config::TestEnvVarGuard::set_path("CODEX_HOME", root.path());
        std::fs::create_dir_all(root.path().join("sessions")).expect("sessions dir");
        let outside = root.path().join("outside.jsonl");
        std::fs::write(&outside, b"{}\n").expect("outside rollout");

        let error = ManualRebindOverrides::validated(
            &ProviderKind::Codex,
            Some(outside.to_str().expect("utf8 path")),
            None,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "codex output_path override must be under an allowed session directory"
        );
    }

    #[test]
    fn health_rebind_override_accepts_codex_persistent_wrapper_output() {
        let root = tempfile::tempdir().expect("AgentDesk root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
        let sessions = crate::services::tmux_common::persistent_sessions_dir()
            .expect("persistent sessions root");
        std::fs::create_dir_all(&sessions).expect("persistent sessions dir");
        let output = sessions.join("codex-wrapper.jsonl");
        std::fs::write(&output, b"{}\n").expect("wrapper output");

        let overrides = ManualRebindOverrides::validated(
            &ProviderKind::Codex,
            Some(output.to_str().expect("utf8 path")),
            None,
        )
        .expect("Codex wrapper output under persistent root");
        assert_eq!(
            overrides.output_path(),
            std::fs::canonicalize(output)
                .expect("canonical wrapper output")
                .to_str()
        );
    }

    #[test]
    fn health_rebind_override_other_providers_require_persistent_root() {
        let root = tempfile::tempdir().expect("AgentDesk root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
        let sessions = crate::services::tmux_common::persistent_sessions_dir()
            .expect("persistent sessions root");
        std::fs::create_dir_all(&sessions).expect("persistent sessions dir");
        let wrapper_output = sessions.join("wrapper.jsonl");
        let outside = root.path().join("outside.jsonl");
        std::fs::write(&wrapper_output, b"{}\n").expect("wrapper output");
        std::fs::write(&outside, b"{}\n").expect("outside output");

        for provider in [
            ProviderKind::Gemini,
            ProviderKind::OpenCode,
            ProviderKind::Qwen,
        ] {
            ManualRebindOverrides::validated(
                &provider,
                Some(wrapper_output.to_str().expect("utf8 path")),
                None,
            )
            .unwrap_or_else(|error| panic!("{} persistent output: {error}", provider.as_str()));
            assert_eq!(
                ManualRebindOverrides::validated(
                    &provider,
                    Some(outside.to_str().expect("utf8 path")),
                    None,
                )
                .unwrap_err(),
                format!(
                    "{} output_path override must be under an allowed session directory",
                    provider.as_str()
                )
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn health_rebind_override_rejects_symlink_escape_from_allowed_root() {
        let root = tempfile::tempdir().expect("Codex home");
        let _env = crate::config::TestEnvVarGuard::set_path("CODEX_HOME", root.path());
        let sessions = root.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        let outside = root.path().join("outside.jsonl");
        let escaped = sessions.join("escaped.jsonl");
        std::fs::write(&outside, b"{}\n").expect("outside output");
        std::os::unix::fs::symlink(&outside, &escaped).expect("escape symlink");

        assert_eq!(
            ManualRebindOverrides::validated(
                &ProviderKind::Codex,
                Some(escaped.to_str().expect("utf8 path")),
                None,
            )
            .unwrap_err(),
            "codex output_path override must be under an allowed session directory"
        );
    }

    #[test]
    fn health_rebind_override_fails_closed_when_allowed_root_is_missing() {
        let root = tempfile::tempdir().expect("isolated root");
        let missing_runtime_root = root.path().join("missing-runtime-root");
        let _env =
            crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", &missing_runtime_root);
        let output = root.path().join("gemini.jsonl");
        std::fs::write(&output, b"{}\n").expect("output");
        assert!(
            allowed_output_roots(&ProviderKind::Gemini)
                .into_iter()
                .all(|candidate| std::fs::canonicalize(candidate).is_err()),
            "test requires every allowed root to be unavailable"
        );

        assert_eq!(
            ManualRebindOverrides::validated(
                &ProviderKind::Gemini,
                Some(output.to_str().expect("utf8 path")),
                None,
            )
            .unwrap_err(),
            "gemini output_path override must be under an allowed session directory"
        );
    }
}
