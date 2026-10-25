//! Typed Codex context preparation and final local evidence revalidation. No source publication.
use super::codex_census as census;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::{SharedData, health::HealthRegistry};
use crate::services::tmux_common::{self as tc, TmuxSourceAuthority};
use crate::services::tui_o::writer::codex_snapshot::{self as model, Observer};
use crate::services::tui_prompt_dedupe::{
    self as dedupe, binding_context as context, binding_events::codex,
};
use context::{BindingContext, SpawnNonceMarker};
use poise::serenity_prelude::ChannelId;
use std::sync::Arc;

pub(super) struct Prepared {
    pub tmux: String,
    pub nonce: String,
    pub observer: Arc<dyn Observer>,
}
struct Read {
    shared: Arc<SharedData>,
    target: model::Target,
    context: BindingContext,
    binding: dedupe::TuiRuntimeBinding,
    current: crate::services::tui_o::shadow::SourceId,
    host: census::HostInventory,
    physical: Option<String>,
    census: String,
    tail: u64,
    log: String,
}

#[derive(Debug)]
pub(crate) enum Component {
    WatcherBinding,
    WatcherOwnership,
    NonceMarker,
    LaunchContext,
    HostIdentity,
    HostedExecution,
    HostInventory,
    RuntimeBinding,
    NativeSource,
    BindingLog,
    PaneCensus,
    PrepareWorker,
}
impl Component {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::WatcherBinding => "watcher_binding",
            Self::WatcherOwnership => "watcher_ownership",
            Self::NonceMarker => "nonce_marker",
            Self::LaunchContext => "launch_context",
            Self::HostIdentity => "host_identity",
            Self::HostedExecution => "hosted_execution",
            Self::HostInventory => "host_inventory",
            Self::RuntimeBinding => "runtime_binding",
            Self::NativeSource => "native_source",
            Self::BindingLog => "binding_log",
            Self::PaneCensus => "pane_census",
            Self::PrepareWorker => "prepare_worker",
        }
    }
}
#[derive(Debug)]
pub(crate) struct Unavailable {
    pub component: Component,
    pub reason: String,
}
impl Unavailable {
    fn new(component: Component, reason: impl Into<String>) -> Self {
        Self {
            component,
            reason: reason.into(),
        }
    }
}

pub(super) async fn prepare(
    _registry: &HealthRegistry,
    shared: Arc<SharedData>,
    channel: u64,
) -> Result<Prepared, Unavailable> {
    prepare_current(shared, channel).await
}

async fn prepare_current(shared: Arc<SharedData>, channel: u64) -> Result<Prepared, Unavailable> {
    let binding = shared
        .tmux_watchers
        .channel_binding(&ChannelId::new(channel))
        .ok_or_else(|| {
            Unavailable::new(
                Component::WatcherBinding,
                "current authoritative watcher binding unavailable",
            )
        })?;
    let tmux = binding.tmux_session_name;
    if binding.owner_channel_id.get() != channel
        || shared.tmux_watchers.owner_channel_for_tmux_session(&tmux)
            != Some(ChannelId::new(channel))
    {
        return Err(Unavailable::new(
            Component::WatcherOwnership,
            "current watcher ownership mismatch",
        ));
    }
    let SpawnNonceMarker::Known(nonce) = context::observe_spawn_nonce_marker(&tmux) else {
        return Err(Unavailable::new(
            Component::NonceMarker,
            "current spawn nonce unavailable",
        ));
    };
    let launch = context::execution_context("codex", &nonce)
        .map_err(|reason| Unavailable::new(Component::LaunchContext, reason))?;
    let host = context::stable_host_identity().ok_or_else(|| {
        Unavailable::new(Component::HostIdentity, "local host identity unavailable")
    })?;
    let (host_path, physical) = if launch.source_policy.is_none() {
        hosted(&shared, channel, &tmux, &nonce)
            .await
            .map_err(|reason| Unavailable::new(Component::HostedExecution, reason))?
    } else {
        (model::HostPath::Legacy, None)
    };
    let target = model::Target {
        channel,
        tmux: tmux.clone(),
        nonce: nonce.clone(),
        host,
        owner_runtime_root: tc::current_tmux_owner_marker(),
        host_path,
    };
    let host = census::host_inventory(physical.as_deref(), &tmux)
        .map_err(|reason| Unavailable::new(Component::HostInventory, reason))?;
    let reader = tokio::task::spawn_blocking(move || {
        let binding = tc::with_tmux_source_authority(&tmux, current_binding)
            .map_err(|reason| Unavailable::new(Component::RuntimeBinding, reason))?;
        let current = (|| {
            let root = launch
                .provider_root
                .as_deref()
                .ok_or("native provider root unavailable")?;
            use crate::services::codex_tui::session::source_observation::{
                CodexHookSourceClaim, CodexRolloutSource, verify_codex_hook_source,
            };
            let verified = verify_codex_hook_source(
                root,
                &CodexHookSourceClaim {
                    session_id: binding
                        .session_id
                        .as_deref()
                        .ok_or("runtime native session unavailable")?,
                    transcript_path: Some(std::path::Path::new(&binding.output_path)),
                    expected_source: CodexRolloutSource::Cli,
                },
            )
            .map_err(|e| format!("native source: {e:?}"))?;
            let (dev, ino) = match verified.identity {
                #[cfg(unix)]
                crate::services::cluster::stream_relay::SourceFileIdentity::Unix { dev, ino } => {
                    (dev, ino)
                }
                _ => return Err("native descriptor unavailable".into()),
            };
            Ok::<_, String>(crate::services::tui_o::shadow::SourceId {
                session_id: verified.session_id,
                path: verified.rollout_path,
                dev,
                ino,
            })
        })()
        .map_err(|reason| Unavailable::new(Component::NativeSource, reason))?;
        let log = codex::observe_channel(channel, census::LIMIT)
            .map_err(|reason| Unavailable::new(Component::BindingLog, reason))?;
        let (_, census) = census::read(&shared, &target, &launch, &log.events, &host)
            .map_err(|reason| Unavailable::new(Component::PaneCensus, reason))?;
        Ok::<_, Unavailable>(Read {
            shared,
            target,
            context: launch,
            binding,
            current,
            host,
            physical,
            census,
            tail: log.tail,
            log: log.digest,
        })
    })
    .await
    .map_err(|reason| Unavailable::new(Component::PrepareWorker, reason.to_string()))??;
    Ok(Prepared {
        tmux: reader.target.tmux.clone(),
        nonce,
        observer: Arc::new(reader),
    })
}

fn current_binding(
    authority: &TmuxSourceAuthority<'_>,
) -> Result<dedupe::TuiRuntimeBinding, String> {
    match dedupe::runtime_observation_under_source_authority(authority) {
        dedupe::RuntimeObservation::Present(binding)
            if binding.runtime_kind == RuntimeHandoffKind::CodexTui =>
        {
            Ok(binding)
        }
        other => Err(format!("runtime observation unavailable: {other:?}")),
    }
}

fn same_pin(left: &dedupe::TuiRuntimeBinding, right: &dedupe::TuiRuntimeBinding) -> bool {
    left.runtime_kind == right.runtime_kind
        && left.session_id == right.session_id
        && left.output_path == right.output_path
        && left.relay_output_path == right.relay_output_path
}

impl Observer for Read {
    fn refresh(&self) -> model::Refresh<'_> {
        Box::pin(async move {
            prepare_current(Arc::clone(&self.shared), self.target.channel)
                .await
                .map(|prepared| prepared.observer)
                .map_err(|error| format!("{}: {}", error.component.as_str(), error.reason))
        })
    }

    fn observe(&self, authority: &TmuxSourceAuthority<'_>) -> Result<model::Inputs, String> {
        if authority.session() != self.target.tmux
            || context::stable_host_identity().as_deref() != Some(self.target.host.as_str())
            || tc::current_tmux_owner_marker() != self.target.owner_runtime_root
            || context::observe_spawn_nonce_marker(authority.session())
                != SpawnNonceMarker::Known(self.target.nonce.clone())
            || context::execution_context("codex", &self.target.nonce)? != self.context
            || std::fs::read_to_string(tc::tmux_owner_path(authority.session()))
                .ok()
                .as_deref()
                .map(str::trim)
                != Some(self.target.owner_runtime_root.as_str())
            || self
                .shared
                .tmux_watchers
                .owner_channel_for_tmux_session(authority.session())
                != Some(ChannelId::new(self.target.channel))
            || !same_pin(&current_binding(authority)?, &self.binding)
        {
            return Err("current Codex publication/context changed".into());
        }
        let host = census::host_inventory(self.physical.as_deref(), &self.target.tmux)?;
        if host.identity != self.host.identity || host.live != self.host.live {
            return Err("Codex host inventory changed after preparation".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(&self.current.path).map_err(|e| e.to_string())?;
            if !meta.is_file() || meta.dev() != self.current.dev || meta.ino() != self.current.ino {
                return Err("current native descriptor changed".into());
            }
        }
        let log = codex::observe_channel(self.target.channel, census::LIMIT)?;
        let (panes, digest) = census::read(
            &self.shared,
            &self.target,
            &self.context,
            &log.events,
            &self.host,
        )?;
        let after = codex::observe_channel(self.target.channel, census::LIMIT)?;
        let (_, after_census) = census::read(
            &self.shared,
            &self.target,
            &self.context,
            &after.events,
            &self.host,
        )?;
        if log.tail != self.tail
            || log.digest != self.log
            || after.digest != log.digest
            || digest != self.census
            || after_census != digest
        {
            return Err("Codex full-tail/census changed during observation".into());
        }
        Ok(model::Inputs {
            target: self.target.clone(),
            context: self.context.clone(),
            current: self.current.clone(),
            events: log.events,
            tail: log.tail,
            log_digest: log.digest,
            census_digest: digest,
            panes,
            coverage: Ok(()),
        })
    }
}

async fn hosted(
    shared: &SharedData,
    channel: u64,
    pane: &str,
    nonce: &str,
) -> Result<(model::HostPath, Option<String>), String> {
    use crate::db::dispatched_session_canonical_identity::{
        CanonicalSessionIdentity, SessionIdentityKind, resolve_session_row_pg,
    };
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedLookup, HostedLookupKey, HostedRecord, HostedState, load_hosted_execution_pg,
    };
    let pool = shared
        .pg_pool
        .as_ref()
        .ok_or("hosted Codex PG unavailable")?;
    let channel_id = channel.to_string();
    let (_, key) = resolve_session_row_pg(
        pool,
        None,
        Some("codex"),
        Some(CanonicalSessionIdentity {
            kind: SessionIdentityKind::DiscordChannel,
            discord_token_hash: &shared.token_hash,
            channel_id: &channel_id,
        }),
    )
    .await
    .map_err(|e| format!("{e:?}"))?
    .ok_or("hosted Codex identity unavailable")?;
    let HostedLookup::Found(row) =
        load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(&key)).await
    else {
        return Err("hosted Codex row unreadable".into());
    };
    let HostedRecord::Known(record) = row.record else {
        return Err("hosted Codex row unknown".into());
    };
    let owner = &record.owner;
    if record.state != HostedState::Bound
        || record.execution_nonce != nonce
        || owner.provider != "codex"
        || owner.logical_key != pane
        || owner.channel_id != channel_id
        || owner.discord_token_hash != shared.token_hash
        || Some(owner.owner_node.as_str()) != crate::config::session_hosts::local_node().as_deref()
        || Some(owner.runtime_root.as_str())
            != crate::config::runtime_root()
                .as_ref()
                .and_then(|r| r.to_str())
        || record
            .location
            .as_ref()
            .is_none_or(|l| l.execution_node != owner.owner_node)
    {
        return Err("hosted Codex ownership mismatch".into());
    }
    let view = crate::services::session_host::herdr_endpoints()
        .view(&record)
        .ok_or("hosted Codex endpoint unavailable")?;
    let location = record
        .location
        .as_ref()
        .ok_or("hosted Codex location unavailable")?;
    Ok((
        model::HostPath::Herdr {
            identity: format!(
                "{}:{}:{}:{}",
                key, location.endpoint_config_key, location.execution_node, nonce
            ),
        },
        Some(view.pane_id().to_owned()),
    ))
}

#[cfg(all(test, unix))]
#[path = "codex_context_tests.rs"]
pub(super) mod tests;
