//! Immutable launch evidence; binding authority remains with the runtime binding.

use crate::services::{platform::tmux::SessionPresence, tmux_common as tc};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

pub(crate) const UNSET_CONTEXT: &str = "unset AGENTDESK_BINDING_CONTEXT\n";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BindingContext {
    pub schema: u32,
    pub provider: String,
    pub created_at: DateTime<Utc>,
    pub execution_nonce: String,
    pub tmux_session: String,
    pub channel_id: Option<u64>,
    pub owner_runtime_root: String,
    pub host: Option<String>,
    pub expected_native_session_id: Option<String>,
    pub launch_mode: String,
    pub provider_root: Option<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct PreparedIncarnation {
    pub context: BindingContext,
    pub path: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ContextPresence {
    Present,
    Absent,
    Unknown,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SpawnNonceMarker {
    Known(String),
    Absent,
    Unreadable,
}

pub(crate) fn stable_host_identity() -> Option<String> {
    let config = crate::config::load_graceful();
    config
        .cluster
        .instance_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            std::env::var("AGENTDESK_INSTANCE_ID")
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
}

fn context_path(provider: &str, nonce: &str) -> io::Result<PathBuf> {
    if !matches!(provider, "claude" | "codex")
        || nonce.len() != 32
        || !nonce.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid context identity",
        ));
    }
    crate::config::runtime_root()
        .map(|root| {
            root.join("runtime/binding_contexts")
                .join(provider)
                .join(format!("{nonce}.json"))
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "runtime root unavailable"))
}

pub(crate) fn context_presence(provider: &str, nonce: &str) -> ContextPresence {
    match context_path(provider, nonce).and_then(fs::metadata) {
        Ok(meta) if meta.is_file() => ContextPresence::Present,
        Err(e) if e.kind() == io::ErrorKind::NotFound => ContextPresence::Absent,
        _ => ContextPresence::Unknown,
    }
}

pub(crate) fn observe_spawn_nonce_marker(tmux: &str) -> SpawnNonceMarker {
    for path in [
        tc::session_temp_path(tmux, "spawn_nonce"),
        tc::legacy_tmp_session_path(tmux, "spawn_nonce"),
    ] {
        match fs::read_to_string(path) {
            Ok(s) if !s.trim().is_empty() => return SpawnNonceMarker::Known(s.trim().to_owned()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            _ => return SpawnNonceMarker::Unreadable,
        }
    }
    SpawnNonceMarker::Absent
}

// The hook capture layer consumes this accessor without consulting mutable markers.
#[allow(dead_code)]
pub(crate) fn context_env() -> Result<PathBuf, std::env::VarError> {
    std::env::var("AGENTDESK_BINDING_CONTEXT").map(PathBuf::from)
}

fn durable_directory(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("context directory has no parent"))?;
    durable_directory(parent)?;
    match fs::create_dir(path) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => (),
        Err(e) => return Err(e),
    }
    fs::File::open(parent)?.sync_all()
}

impl PreparedIncarnation {
    pub(crate) fn prepare(
        provider: &str,
        tmux: &str,
        channel_id: Option<u64>,
        expected: Option<&str>,
        resume: bool,
    ) -> Result<Self, String> {
        let context = BindingContext {
            schema: 1,
            provider: provider.to_owned(),
            created_at: Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: tmux.to_owned(),
            channel_id,
            owner_runtime_root: tc::current_tmux_owner_marker(),
            host: stable_host_identity(),
            expected_native_session_id: expected.map(str::to_owned),
            launch_mode: if resume { "resume" } else { "fresh" }.to_owned(),
            provider_root: (provider == "claude")
                .then(
                    crate::services::claude_tui::hook_output_guard::configured_claude_projects_root,
                )
                .flatten(),
        };
        sweep(
            provider,
            Utc::now(),
            32,
            crate::services::platform::tmux::session_presence,
        );
        Self::create(context).map_err(|e| format!("create binding context: {e}"))
    }

    pub(crate) fn create(context: BindingContext) -> io::Result<Self> {
        let path = context_path(&context.provider, &context.execution_nonce)?;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("context path has no parent"))?;
        durable_directory(parent)?;
        let temp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
        let result = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(&serde_json::to_vec(&context)?)?;
            #[cfg(test)]
            creation_fault("file")?;
            file.sync_all()?;
            #[cfg(test)]
            creation_fault("link")?;
            fs::hard_link(&temp, &path)?;
            fs::remove_file(&temp)?;
            #[cfg(test)]
            creation_fault("parent")?;
            fs::File::open(parent)?.sync_all()
        })();
        let _ = fs::remove_file(&temp);
        result?;
        Ok(Self { context, path })
    }

    pub(crate) fn env_lines(&self) -> String {
        format!(
            "{UNSET_CONTEXT}export AGENTDESK_BINDING_CONTEXT={}\n",
            crate::services::process::shell_escape(&self.path.to_string_lossy())
        )
    }

    pub(crate) fn validate(&self, tmux: &str) -> io::Result<()> {
        let valid = context_presence(&self.context.provider, &self.context.execution_nonce)
            == ContextPresence::Present
            && context_path(&self.context.provider, &self.context.execution_nonce)
                .ok()
                .as_ref()
                == Some(&self.path)
            && fs::read(&self.path)
                .ok()
                .and_then(|b| serde_json::from_slice::<BindingContext>(&b).ok())
                .is_some_and(|ctx| {
                    ctx.schema == 1 && ctx.tmux_session == tmux && ctx == self.context
                });
        if valid {
            Ok(())
        } else {
            Err(io::Error::other("ContextNotPublishable"))
        }
    }
}

fn sweep(
    provider: &str,
    now: DateTime<Utc>,
    budget: usize,
    probe: impl Fn(&str) -> SessionPresence,
) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let Ok(path) = context_path(provider, &"0".repeat(32)) else {
        return;
    };
    let Some(parent) = path.parent() else { return };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    if paths.is_empty() {
        return;
    }
    let start = NEXT.fetch_add(budget, Ordering::Relaxed) % paths.len();
    for path in paths
        .iter()
        .cycle()
        .skip(start)
        .take(budget.min(paths.len()))
    {
        let Some(ctx) = fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<BindingContext>(&b).ok())
        else {
            continue;
        };
        if now.signed_duration_since(ctx.created_at) < chrono::Duration::days(7)
            || context_path(&ctx.provider, &ctx.execution_nonce)
                .ok()
                .as_ref()
                != Some(path)
        {
            continue;
        }
        let presence = probe(&ctx.tmux_session);
        tc::with_tmux_source_authority(&ctx.tmux_session, |_| {
            let retired = match observe_spawn_nonce_marker(&ctx.tmux_session) {
                SpawnNonceMarker::Known(n) => n != ctx.execution_nonce,
                SpawnNonceMarker::Absent => presence == SessionPresence::Missing,
                SpawnNonceMarker::Unreadable => false,
            };
            if retired {
                let _ = fs::remove_file(path);
            }
        });
    }
}

#[cfg(test)]
thread_local! { pub(crate) static CREATE_FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
fn creation_fault(step: &str) -> io::Result<()> {
    if CREATE_FAULT.with(|f| f.get() == Some(step)) {
        Err(io::Error::other(format!("injected {step}")))
    } else {
        Ok(())
    }
}
