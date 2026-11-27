use std::path::{Path, PathBuf};

use poise::serenity_prelude::ChannelId;

use super::{SharedData, session_locator};
use crate::db::dispatched_sessions::{ProviderSessionIds, load_provider_session_ids_pg};
use crate::db::session_transcripts::PreviousProviderSession;
use crate::services::provider::ProviderKind;
use crate::services::session_selector_validity::transcript::selected_provider_session;
use crate::services::tui_prompt_dedupe::{binding_context, hosted_source};

#[derive(Default)]
pub(super) struct Selection<'a> {
    pub(super) session_key: Option<&'a str>,
    pub(super) session_id: Option<&'a str>,
    pub(super) cwd: Option<&'a Path>,
    pub(super) logical: Option<&'a str>,
}

pub(super) async fn for_channel(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    selected: Selection<'_>,
) -> Result<Option<PreviousProviderSession>, String> {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return Ok(None);
    };
    let key = match selected.session_key.and_then(nonempty) {
        Some(key) => Some(key.to_owned()),
        None => {
            match session_locator::current_row(pool, &shared.token_hash, provider, channel.get())
                .await?
            {
                Some((_, key)) => Some(key),
                None => {
                    let logical = match selected.logical {
                        Some(logical) => Some(logical.to_owned()),
                        None => channel_logical_name(&*shared.core.lock().await, provider, channel),
                    };
                    logical.map(|name| {
                        super::adk_session::build_namespaced_session_key(
                            &shared.token_hash,
                            provider,
                            &name,
                        )
                    })
                }
            }
        }
    };
    let selected = Selection {
        session_key: key.as_deref(),
        ..selected
    };
    for_clear_key(shared, provider, channel, selected).await
}

pub(super) async fn for_clear_key(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    selected: Selection<'_>,
) -> Result<Option<PreviousProviderSession>, String> {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return Ok(None);
    };
    let stored = match selected.session_key {
        Some(key) => load_provider_session_ids_pg(pool, key, Some(provider.as_str())).await?,
        None => None,
    };
    let core = shared.core.lock().await;
    let session = core.sessions.get(&channel);
    let memory = session.and_then(|s| s.session_id.as_deref());
    let cwd = session
        .and_then(|s| s.current_path.as_deref())
        .map(Path::new);
    let logical = channel_logical_name(&core, provider, channel);
    let fallback = Selection {
        session_key: None,
        session_id: selected.session_id.or(memory),
        cwd: selected.cwd.or(cwd),
        logical: selected.logical.or(logical.as_deref()),
    };
    let capture = load_capture(stored, fallback);
    drop(core);
    capture.previous(provider, channel, None).await
}

fn channel_logical_name(
    core: &super::CoreState,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Option<String> {
    core.sessions
        .get(&channel)
        .and_then(|session| session.channel_name.as_deref())
        .map(|name| provider.build_tmux_session_name(name))
}

pub(super) fn load_capture(stored: Option<ProviderSessionIds>, fallback: Selection<'_>) -> Capture {
    let stored_logical = stored.as_ref().and_then(|row| {
        super::session_identity::tmux_name_from_session_key(&row.resolved_session_key)
    });
    Capture {
        stored,
        fallback_id: fallback.session_id.and_then(nonempty).map(str::to_owned),
        fallback_cwd: fallback.cwd.map(Path::to_path_buf),
        logical: stored_logical.or_else(|| fallback.logical.map(str::to_owned)),
    }
}

pub(super) struct Capture {
    stored: Option<ProviderSessionIds>,
    fallback_id: Option<String>,
    fallback_cwd: Option<PathBuf>,
    logical: Option<String>,
}

impl Capture {
    pub(super) async fn previous(
        mut self,
        provider: &ProviderKind,
        channel: ChannelId,
        hosted_execution: Option<(&str, &str)>,
    ) -> Result<Option<PreviousProviderSession>, String> {
        let provider = provider.clone();
        let nonce = hosted_execution.map(|(logical, nonce)| {
            self.logical = Some(logical.to_owned());
            nonce.to_owned()
        });
        tokio::task::spawn_blocking(move || self.select(&provider, channel, nonce.as_deref()))
            .await
            .map_err(|error| error.to_string())
    }

    fn select(
        self,
        provider: &ProviderKind,
        channel: ChannelId,
        execution_nonce: Option<&str>,
    ) -> Option<PreviousProviderSession> {
        let context = self
            .logical
            .as_deref()
            .and_then(|logical| execution_context(provider, channel, logical, execution_nonce));
        let root = context
            .as_ref()
            .and_then(|context| context.provider_root.as_deref());
        // Compare transcript activity independently of whether this execution can resume.
        let id = self
            .stored
            .as_ref()
            .and_then(|ids| {
                selected_provider_session(Some(provider.as_str()), ids, root.and_then(Path::parent))
            })
            .or(self.fallback_id.as_deref())?;
        let cwd = self
            .stored
            .as_ref()
            .and_then(|ids| ids.cwd.as_deref())
            .and_then(nonempty)
            .map(Path::new)
            .or(self.fallback_cwd.as_deref());
        let path = context
            .as_ref()
            .and_then(|context| hosted_source::capture_session_in_context(context, id))
            .and_then(|pin| original_file(provider, id, &pin.source.path))
            .or_else(|| resolve_path(provider, id, cwd, root));
        Some(PreviousProviderSession {
            provider: provider.as_str().to_owned(),
            session_id: id.to_owned(),
            path,
        })
    }
}

fn nonempty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn execution_context(
    provider: &ProviderKind,
    channel: ChannelId,
    logical: &str,
    nonce: Option<&str>,
) -> Option<binding_context::BindingContext> {
    use crate::services::tmux_common;
    tmux_common::with_tmux_source_authority(logical, |_| {
        let nonce = match nonce {
            Some(nonce) => nonce.to_owned(),
            None => match binding_context::observe_spawn_nonce_marker(logical) {
                binding_context::SpawnNonceMarker::Known(nonce) => nonce,
                _ => return None,
            },
        };
        let context = binding_context::execution_context(provider.as_str(), &nonce).ok()?;
        (context.channel_id == Some(channel.get())
            && context.tmux_session == logical
            && context.owner_runtime_root == tmux_common::current_tmux_owner_marker())
        .then_some(context)
    })
}

fn resolve_path(
    provider: &ProviderKind,
    id: &str,
    cwd: Option<&Path>,
    root: Option<&Path>,
) -> Option<PathBuf> {
    uuid::Uuid::parse_str(id).ok()?;
    let path = match provider {
        ProviderKind::Claude => {
            crate::services::claude_tui::transcript_tail::claude_transcript_path(
                cwd?,
                id,
                root.and_then(Path::parent),
            )
            .ok()
        }
        ProviderKind::Codex => {
            use crate::services::codex_tui::rollout_tail::{
                find_rollout_by_session_id, find_rollout_by_session_id_under,
            };
            match root {
                Some(root) => find_rollout_by_session_id_under(root, id),
                None => find_rollout_by_session_id(id),
            }
        }
        _ => None,
    }?;
    original_file(provider, id, &path)
}

fn original_file(provider: &ProviderKind, id: &str, path: &Path) -> Option<PathBuf> {
    let path = path.canonicalize().ok()?;
    path.to_str()?;
    if !path.is_file() || crate::config::test_instance::environment::validate_path(&path).is_err() {
        return None;
    }
    let matches = match provider {
        ProviderKind::Claude => {
            path.file_stem().and_then(std::ffi::OsStr::to_str) == Some(id)
                && path.extension().is_some_and(|ext| ext == "jsonl")
        }
        ProviderKind::Codex => {
            crate::services::codex_tui::rollout_index::read_rollout_session_meta(&path)
                .is_some_and(|meta| meta.id.as_deref() == Some(id) && !meta.is_subagent())
        }
        _ => false,
    };
    matches.then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previous_transcript_resolves_the_native_file_for_each_provider() {
        let fixture = tempfile::tempdir().unwrap();
        let cwd = fixture.path().join("workspace");
        std::fs::create_dir(&cwd).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let claude_root = fixture.path().join("claude/projects");
        let claude_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            &cwd,
            &id,
            claude_root.parent(),
        )
        .unwrap();
        let codex_root = fixture.path().join("codex/sessions");
        let codex_path = codex_root
            .join("2026/11/27")
            .join(format!("rollout-fixture-{id}.jsonl"));
        let codex_header = serde_json::json!({
            "type": "session_meta",
            "payload": { "id": id, "cwd": cwd, "source": "cli" }
        });
        for (provider, root, path, content) in [
            (
                ProviderKind::Claude,
                &claude_root,
                &claude_path,
                "{}".to_owned(),
            ),
            (
                ProviderKind::Codex,
                &codex_root,
                &codex_path,
                codex_header.to_string(),
            ),
        ] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
            assert_eq!(
                resolve_path(&provider, &id, Some(&cwd), Some(root)),
                Some(path.canonicalize().unwrap()),
            );
            std::fs::remove_file(path).unwrap();
            assert_eq!(resolve_path(&provider, &id, Some(&cwd), Some(root)), None);
        }
    }
}
