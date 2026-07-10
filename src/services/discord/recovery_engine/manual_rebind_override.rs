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
    if provider == &ProviderKind::Claude && !is_under_claude_projects_dir(&canonical) {
        return Err(
            "Claude output_path override must be under a Claude projects directory".to_string(),
        );
    }
    Ok(canonical.display().to_string())
}

fn is_under_claude_projects_dir(path: &Path) -> bool {
    claude_projects_dir_candidates()
        .into_iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .any(|root| path.starts_with(root))
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

    struct EnvGuard(Option<std::ffi::OsString>);

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", value) },
                None => unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") },
            }
        }
    }

    fn set_claude_home(path: &Path) -> EnvGuard {
        let previous = std::env::var_os("CLAUDE_CONFIG_DIR");
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", path) };
        EnvGuard(previous)
    }

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
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("Claude home");
        let _env = set_claude_home(root.path());
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
    fn health_rebind_override_accepts_claude_projects_file_and_canonicalizes_it() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("Claude home");
        let _env = set_claude_home(root.path());
        let project = root.path().join("projects/-tmp-agentdesk");
        std::fs::create_dir_all(&project).expect("project dir");
        let transcript = project.join("4c474e5d-37e7-4b6a-bcf7-d68854a31c49.jsonl");
        std::fs::write(&transcript, b"{}\n").expect("transcript");

        let overrides = ManualRebindOverrides::validated(
            &ProviderKind::Claude,
            Some(transcript.to_str().expect("utf8 path")),
            Some("4c474e5d-37e7-4b6a-bcf7-d68854a31c49"),
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
    }
}
