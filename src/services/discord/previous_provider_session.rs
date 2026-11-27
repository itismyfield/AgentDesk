use std::path::{Path, PathBuf};

use poise::serenity_prelude::ChannelId;

use super::{SharedData, adk_session, session_locator};
use crate::db::session_transcripts::PreviousProviderSession;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::provider::ProviderKind;
use crate::services::tui_prompt_dedupe::{binding_context, runtime_binding_for_tmux_session};

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
) -> Option<PreviousProviderSession> {
    let pool = shared.pg_pool.as_ref()?;
    let (cached, cwd, logical) = {
        let core = shared.core.lock().await;
        let session = core.sessions.get(&channel);
        (
            session.and_then(|s| s.session_id.clone()),
            session.and_then(|s| s.current_path.as_deref().map(PathBuf::from)),
            session
                .and_then(|s| s.channel_name.as_deref())
                .map(|name| provider.build_tmux_session_name(name)),
        )
    };
    let logical = selected.logical.or(logical.as_deref());
    let selected_id = selected.session_id.or(cached.as_deref()).and_then(nonempty);
    let cwd = selected.cwd.or(cwd.as_deref());
    if let (Some(id), Some(cwd)) = (selected_id, cwd) {
        return capture(provider, channel, id, Some(cwd), logical, None).await;
    }

    let key = match selected.session_key.and_then(nonempty) {
        Some(key) => Some(key.to_owned()),
        None => session_locator::current_row(pool, &shared.token_hash, provider, channel.get())
            .await
            .ok()
            .flatten()
            .map(|(_, key)| key)
            .or_else(|| {
                logical.map(|name| {
                    adk_session::build_namespaced_session_key(&shared.token_hash, provider, name)
                })
            }),
    };
    let stored = match key {
        Some(key) => crate::db::dispatched_sessions::load_provider_session_ids_pg(
            pool,
            &key,
            Some(provider.as_str()),
        )
        .await
        .ok()
        .flatten(),
        None => None,
    };
    let id = selected_id.or_else(|| {
        let row = stored.as_ref()?;
        row.raw_provider_session_id
            .as_deref()
            .and_then(nonempty)
            .or_else(|| row.claude_session_id.as_deref().and_then(nonempty))
    })?;
    let cwd = cwd.or_else(|| {
        stored
            .as_ref()?
            .cwd
            .as_deref()
            .and_then(nonempty)
            .map(Path::new)
    });
    let stored_logical = stored.as_ref().and_then(|row| {
        super::session_identity::tmux_name_from_session_key(&row.resolved_session_key)
    });
    capture(
        provider,
        channel,
        id,
        cwd,
        logical.or(stored_logical.as_deref()),
        None,
    )
    .await
}

fn nonempty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

pub(super) async fn capture(
    provider: &ProviderKind,
    channel: ChannelId,
    id: &str,
    cwd: Option<&Path>,
    logical: Option<&str>,
    nonce: Option<&str>,
) -> Option<PreviousProviderSession> {
    let id = nonempty(id)?.to_owned();
    let mut previous = PreviousProviderSession {
        provider: provider.as_str().to_owned(),
        session_id: id.clone(),
        path: None,
    };
    if uuid::Uuid::parse_str(&id).is_ok() {
        let (provider, cwd, logical, nonce) = (
            provider.clone(),
            cwd.map(Path::to_path_buf),
            logical.map(str::to_owned),
            nonce.map(str::to_owned),
        );
        previous.path = tokio::task::spawn_blocking(move || {
            let root = logical.as_deref().and_then(|name| {
                let nonce =
                    nonce.or_else(|| match binding_context::observe_spawn_nonce_marker(name) {
                        binding_context::SpawnNonceMarker::Known(nonce) => Some(nonce),
                        _ => None,
                    })?;
                let context = binding_context::execution_context(provider.as_str(), &nonce).ok()?;
                (context.channel_id == Some(channel.get()) && context.tmux_session == name)
                    .then_some(context.provider_root)
                    .flatten()
            });
            resolve_path(
                &provider,
                &id,
                cwd.as_deref(),
                logical.as_deref(),
                root.as_deref(),
            )
        })
        .await
        .ok()
        .flatten();
    }
    Some(previous)
}

fn resolve_path(
    provider: &ProviderKind,
    id: &str,
    cwd: Option<&Path>,
    logical: Option<&str>,
    root: Option<&Path>,
) -> Option<PathBuf> {
    let kind = match provider {
        ProviderKind::Claude => RuntimeHandoffKind::ClaudeTui,
        ProviderKind::Codex => RuntimeHandoffKind::CodexTui,
        _ => return None,
    };
    let bound = logical
        .and_then(runtime_binding_for_tmux_session)
        .filter(|binding| {
            binding.runtime_kind == kind && binding.session_id.as_deref() == Some(id)
        });
    if let Some(path) =
        bound.and_then(|binding| original_file(provider, id, Path::new(&binding.output_path)))
    {
        return Some(path);
    }

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
                resolve_path(&provider, &id, Some(&cwd), None, Some(root)),
                Some(path.canonicalize().unwrap()),
            );
            std::fs::remove_file(path).unwrap();
            assert_eq!(
                resolve_path(&provider, &id, Some(&cwd), None, Some(root)),
                None
            );
        }
    }
}
