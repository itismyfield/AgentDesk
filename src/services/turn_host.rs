//! Which host runs a provider turn, judged once before spawn. A channel configured for Herdr, or
//! one whose row still holds a Herdr execution, is never handed to tmux or the process driver.

use sqlx::PgPool;

use crate::config::session_hosts::{self, ChannelEndpoint};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLookup, HostedLookupKey, HostedObservation, HostedOwner, HostedRecord,
    HostedState, load_hosted_execution_pg,
};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::codex_tui::canary::CANARY_TMUX;
use crate::services::herdr_admission::{self, StopCause};
use crate::services::herdr_launch::{HerdrLaunchEndpoint, o_bootstrap_base, o_writer_ready};
use crate::services::provider::ProviderKind;
use crate::services::tui_prompt_dedupe::clear_relaunch::herdr_bootstrap;

#[derive(Debug)]
pub(crate) enum TurnHost {
    /// The existing path, unchanged.
    Tmux,
    Herdr(Box<HerdrTurnPlan>),
    /// The channel left Herdr while its local Claude execution still holds the row, or has not
    /// finished moving back: that execution ends and its session returns to tmux first.
    Return(Box<ReturnPlan>),
    Refused(HerdrRefusal),
}

#[derive(Debug)]
pub(crate) struct ReturnPlan {
    pub row: HostedObservation,
    pub record: HostedExecution,
}

#[derive(Debug)]
pub(crate) struct HerdrTurnPlan {
    pub endpoint: HerdrLaunchEndpoint,
    /// `None` while the channel has no sessions row yet.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub row: Option<HostedObservation>,
    /// The retired canary's launch its blocked writer waits for; the launch reads the old pane's
    /// vacancy again before its Pending row.
    pub bootstrap: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HerdrRefusal {
    /// The row holds a Pending or Bound Herdr execution (`None`: an unreadable record) but the
    /// channel is not configured.
    HostedHerdrUnconfigured {
        state: Option<HostedState>,
    },
    ProviderUnsupported {
        provider: String,
    },
    EndpointNotLocal {
        node: String,
    },
    HostedRowUnreadable {
        detail: String,
    },
    AdmissionStopped {
        cause: StopCause,
    },
    OWriterNotReady,
    ExecutorNotWired,
}

impl std::fmt::Display for HerdrRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("herdr turn refused: ")?;
        match self {
            Self::HostedHerdrUnconfigured { state } => {
                write!(f, "hosted_herdr_unconfigured(state={state:?})")
            }
            Self::ProviderUnsupported { provider } => {
                write!(f, "provider_unsupported({provider})")
            }
            Self::EndpointNotLocal { node } => write!(f, "endpoint_not_local({node})"),
            Self::HostedRowUnreadable { detail } => write!(f, "hosted_row_unreadable({detail})"),
            Self::AdmissionStopped { cause } => write!(f, "admission_stopped({})", cause.as_str()),
            Self::OWriterNotReady => f.write_str("o_writer_not_ready"),
            Self::ExecutorNotWired => f.write_str("executor_not_wired"),
        }
    }
}

/// The sessions row (`None`: no row yet), or why it could not be read; read before any tmux or
/// Herdr I/O.
type RowRead = Result<Option<HostedObservation>, String>;

/// The Herdr execution the row still holds: Pending, Bound or an unreadable record.
fn herdr_trace(row: &RowRead) -> Option<Option<HostedState>> {
    let Ok(Some(observed)) = row else {
        return None;
    };
    match &observed.record {
        HostedRecord::Legacy => None,
        HostedRecord::Known(record) if record.state == HostedState::Retired => None,
        HostedRecord::Known(record) => Some(Some(record.state)),
        HostedRecord::Unknown(_) => Some(None),
    }
}

/// A Bound Claude execution on this node, or a retired one whose move back is unfinished: its
/// marker still names Herdr or this process still lists it there. Pending rows are not.
fn return_plan(provider: &ProviderKind, row: &RowRead) -> Option<ReturnPlan> {
    let Ok(Some(observed)) = row else {
        return None;
    };
    let HostedRecord::Known(record) = &observed.record else {
        return None;
    };
    let local = session_hosts::local_node();
    let at = record.location.as_ref()?;
    let ours = *provider == ProviderKind::Claude
        && record.owner.provider == "claude"
        && local.as_deref() == Some(at.execution_node.as_str());
    let due = match record.state {
        HostedState::Bound => true,
        HostedState::Retired => !returned(&record.owner.logical_key, &record.execution_nonce),
        HostedState::Pending => false,
    };
    let (row, record) = (observed.clone(), record.clone());
    (ours && due).then_some(ReturnPlan { row, record })
}

/// Whether execution `nonce`'s session finished moving back: its marker names tmux and this
/// process no longer lists it as a Herdr pane.
#[cfg(unix)]
pub(crate) fn returned(logical: &str, nonce: &str) -> bool {
    crate::services::claude::host_gate::tmux_marked(logical)
        && crate::services::tui_prompt_dedupe::herdr_listed_execution(logical).as_deref()
            != Some(nonce)
}

#[cfg(not(unix))]
pub(crate) fn returned(_logical: &str, _nonce: &str) -> bool {
    true
}

async fn read_row(pool: Option<&PgPool>, session_key: Option<&str>) -> RowRead {
    let (Some(pool), Some(session_key)) = (pool, session_key) else {
        return Err("no database or session key".into());
    };
    match load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(session_key)).await {
        HostedLookup::Found(observed) => Ok(Some(observed)),
        HostedLookup::Missing => Ok(None),
        HostedLookup::Unknown(detail) => Err(detail),
        HostedLookup::Conflict(kind) => Err(format!("{kind:?}")),
    }
}

/// Called again right before spawn, after [`refusal_before_turn`]. Admission and O readiness are
/// read only for a configured channel.
pub(crate) async fn for_turn(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    channel_id: u64,
    session_key: Option<&str>,
) -> TurnHost {
    resolve(pool, provider, channel_id, session_key, true).await
}

/// Reads the selected host and its current row without admission or output-writer gates.
#[cfg(unix)]
pub(crate) async fn for_observation(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    channel_id: u64,
    session_key: Option<&str>,
) -> TurnHost {
    resolve(pool, provider, channel_id, session_key, false).await
}

async fn resolve(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    channel_id: u64,
    session_key: Option<&str>,
    execution: bool,
) -> TurnHost {
    let configured_endpoint = |channel_id| session_hosts::herdr_endpoint(channel_id);
    if let Some(endpoint) = configured_endpoint(channel_id) {
        return configured_turn(
            pool,
            provider,
            channel_id,
            session_key,
            (endpoint, execution),
            None,
        )
        .await;
    }
    let row = read_row(pool, session_key).await;
    let policy = crate::services::session_host::surface_policy::policy_channel(channel_id);
    if policy != channel_id
        && let Some(endpoint) = configured_endpoint(policy)
    {
        if is_live_thread_endpoint(channel_id, provider, session_key, &row, &endpoint) {
            return configured_turn(
                pool,
                provider,
                channel_id,
                session_key,
                (endpoint, execution),
                Some(row),
            )
            .await;
        }
        if let Err(detail) = &row {
            return TurnHost::Refused(HerdrRefusal::HostedRowUnreadable {
                detail: detail.clone(),
            });
        }
        return match herdr_trace(&row) {
            Some(state) => TurnHost::Refused(HerdrRefusal::HostedHerdrUnconfigured { state }),
            None => TurnHost::Tmux,
        };
    }
    if let Some(plan) = return_plan(provider, &row) {
        return TurnHost::Return(Box::new(plan));
    }
    match herdr_trace(&row) {
        Some(state) => TurnHost::Refused(HerdrRefusal::HostedHerdrUnconfigured { state }),
        None => TurnHost::Tmux,
    }
}

/// A checked child can keep its current Herdr pane under its parent's configured endpoint.
fn is_live_thread_endpoint(
    channel: u64,
    provider: &ProviderKind,
    key: Option<&str>,
    row: &RowRead,
    endpoint: &ChannelEndpoint,
) -> bool {
    let Some(HostedRecord::Known(record)) = row
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .map(|row| &row.record)
    else {
        return false;
    };
    expected_owner(provider, channel, key).as_ref() == Some(&record.owner)
        && bound_thread_endpoint_matches(record, channel, endpoint)
}

fn bound_thread_endpoint_matches(
    record: &HostedExecution,
    channel: u64,
    endpoint: &ChannelEndpoint,
) -> bool {
    record.state == HostedState::Bound
        && record.owner.channel_id == channel.to_string()
        && record.location.as_ref().is_some_and(|location| {
            location.endpoint_config_key == endpoint.key
                && location.execution_node == endpoint.execution_node
                && location.socket_addr == endpoint.socket_path.to_string_lossy()
                && location.named_session == endpoint.herdr_session
        })
}

/// Whether the actual destination can have a local Herdr execution to inspect.
pub(crate) fn has_effective_herdr_endpoint(channel: u64) -> bool {
    session_hosts::has_herdr_endpoint(channel)
        || session_hosts::has_herdr_endpoint(
            crate::services::session_host::surface_policy::policy_channel(channel),
        )
}

/// A canonical Bound child can continue with the same endpoint rule as turn routing.
pub(crate) fn bound_herdr_endpoint(
    channel: u64,
    record: &HostedExecution,
) -> Option<ChannelEndpoint> {
    session_hosts::herdr_endpoint(channel).or_else(|| {
        let parent = crate::services::session_host::surface_policy::policy_channel(channel);
        let endpoint = session_hosts::herdr_endpoint(parent)?;
        bound_thread_endpoint_matches(record, channel, &endpoint).then_some(endpoint)
    })
}

/// Before resetting, reconciling or clearing a turn, refuse a configured Herdr session.
/// An unconfigured channel passes without I/O.
pub(crate) async fn refusal_before_turn<F>(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    channel_id: u64,
    session_key: impl FnOnce() -> F,
) -> Option<HerdrRefusal>
where
    F: std::future::Future<Output = Option<String>>,
{
    let policy = crate::services::session_host::surface_policy::policy_channel(channel_id);
    if !session_hosts::has_herdr_endpoint(channel_id)
        && (policy == channel_id || !session_hosts::has_herdr_endpoint(policy))
    {
        return None;
    }
    let session_key = session_key().await;
    match for_turn(pool, provider, channel_id, session_key.as_deref()).await {
        TurnHost::Tmux => None,
        TurnHost::Refused(refusal) => Some(refusal),
        TurnHost::Herdr(_) => Some(HerdrRefusal::ExecutorNotWired),
        TurnHost::Return(_) => None,
    }
}

/// The intake's first judgement: a configured turn only the missing executor refused passes while
/// its provider's Herdr switch is on.
pub(crate) async fn intake_refusal_before_turn<F>(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    channel_id: u64,
    session_key: impl FnOnce() -> F,
) -> Option<HerdrRefusal>
where
    F: std::future::Future<Output = Option<String>>,
{
    match refusal_before_turn(pool, provider, channel_id, session_key).await {
        Some(HerdrRefusal::ExecutorNotWired) if herdr_turn_switched_on_for(provider) => None,
        judged => judged,
    }
}

#[cfg(test)]
thread_local! {
    static FORCED_SWITCH: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static FORCED_CODEX_SWITCH: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether a configured channel's turn of `provider` runs on Herdr; any other provider never does.
pub(crate) fn herdr_turn_switched_on_for(provider: &ProviderKind) -> bool {
    match provider {
        ProviderKind::Claude => herdr_turn_switched_on(),
        ProviderKind::Codex => {
            #[cfg(test)]
            if let Some(forced) = FORCED_CODEX_SWITCH.with(std::cell::Cell::get) {
                return forced;
            }
            cfg!(unix)
                && crate::config_live_reload::current()
                    .and_then(|config| config.runtime.herdr_codex_turn_enabled)
                    .unwrap_or(false)
        }
        _ => false,
    }
}

/// Whether a configured channel's Claude turn runs on Herdr; unset or false keeps it refused.
pub(crate) fn herdr_turn_switched_on() -> bool {
    #[cfg(test)]
    if let Some(forced) = FORCED_SWITCH.with(std::cell::Cell::get) {
        return forced;
    }
    cfg!(unix)
        && crate::config_live_reload::current()
            .and_then(|config| config.runtime.herdr_turn_enabled)
            .unwrap_or(false)
}

/// Stands in for the live switch on this thread until dropped.
#[cfg(test)]
pub(crate) struct ForcedSwitch(Option<bool>);

#[cfg(test)]
pub(crate) fn force_switch_for_test(on: Option<bool>) -> ForcedSwitch {
    ForcedSwitch(FORCED_SWITCH.with(|cell| cell.replace(on)))
}

#[cfg(test)]
impl Drop for ForcedSwitch {
    fn drop(&mut self) {
        FORCED_SWITCH.with(|cell| cell.set(self.0));
    }
}

/// Stands in for the live Codex switch on this thread until dropped.
#[cfg(test)]
pub(crate) struct ForcedCodexSwitch(Option<bool>);

#[cfg(test)]
pub(crate) fn force_codex_switch_for_test(on: Option<bool>) -> ForcedCodexSwitch {
    ForcedCodexSwitch(FORCED_CODEX_SWITCH.with(|cell| cell.replace(on)))
}

#[cfg(test)]
impl Drop for ForcedCodexSwitch {
    fn drop(&mut self) {
        FORCED_CODEX_SWITCH.with(|cell| cell.set(self.0));
    }
}

async fn configured_turn(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    channel_id: u64,
    session_key: Option<&str>,
    (endpoint, execution): (ChannelEndpoint, bool),
    observed: Option<RowRead>,
) -> TurnHost {
    match configured_plan(
        pool,
        provider,
        channel_id,
        session_key,
        (endpoint, execution),
        observed,
    )
    .await
    {
        Ok(plan) => TurnHost::Herdr(Box::new(plan)),
        Err(refusal) => TurnHost::Refused(refusal),
    }
}

async fn configured_plan(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    channel_id: u64,
    session_key: Option<&str>,
    (endpoint, execution): (ChannelEndpoint, bool),
    observed: Option<RowRead>,
) -> Result<HerdrTurnPlan, HerdrRefusal> {
    // Execution checks the provider switch before any read; observation never starts a turn.
    let kind = match provider {
        ProviderKind::Claude => RuntimeHandoffKind::ClaudeTui,
        ProviderKind::Codex if !execution || herdr_turn_switched_on_for(provider) => {
            RuntimeHandoffKind::CodexTui
        }
        _ => {
            let provider = provider.as_str().to_owned();
            return Err(HerdrRefusal::ProviderUnsupported { provider });
        }
    };
    if session_hosts::local_node().as_deref() != Some(endpoint.execution_node.as_str()) {
        let node = endpoint.execution_node;
        return Err(HerdrRefusal::EndpointNotLocal { node });
    }
    let read = match observed {
        Some(read) => read,
        None => read_row(pool, session_key).await,
    };
    if herdr_trace(&read) == Some(None) {
        let detail = "hosted execution record is unreadable".into();
        return Err(HerdrRefusal::HostedRowUnreadable { detail });
    }
    let row = read.map_err(|detail| HerdrRefusal::HostedRowUnreadable { detail })?;
    let bootstrap = if execution {
        herdr_admission::check().map_err(|cause| HerdrRefusal::AdmissionStopped { cause })?;
        if o_writer_ready(channel_id, kind) {
            false
        } else if kind == RuntimeHandoffKind::CodexTui
            && bootstrap_eligible(provider, channel_id, session_key, row.as_ref()).is_ok()
        {
            true
        } else {
            return Err(HerdrRefusal::OWriterNotReady);
        }
    } else {
        false
    };
    let endpoint = launch_endpoint(endpoint);
    Ok(HerdrTurnPlan {
        endpoint,
        row,
        bootstrap,
    })
}

/// Whether the pin-blocked canary writer's Herdr launch may start: a vacant old pane, a live owned
/// actor, a seeded O store and this owner's Retired row, neither admitted nor held.
fn bootstrap_eligible(
    provider: &ProviderKind,
    channel: u64,
    session_key: Option<&str>,
    row: Option<&HostedObservation>,
) -> Result<(), &'static str> {
    let pin = herdr_bootstrap::local_eligible(channel)?;
    if let Some(refusal) = writer_refusal(channel) {
        return Err(refusal);
    }
    let Some(HostedRecord::Known(record)) = row.map(|observed| &observed.record) else {
        return Err("row_not_known");
    };
    if record.state != HostedState::Retired {
        return Err("row_not_retired");
    }
    if record.owner.logical_key != CANARY_TMUX
        || expected_owner(provider, channel, session_key).as_ref() != Some(&record.owner)
    {
        return Err("owner_differs");
    }
    if record.execution_nonce == pin {
        return Err("row_is_pin");
    }
    // A withheld listing of this very execution is what a failed marker write leaves behind.
    match crate::services::tui_prompt_dedupe::herdr_listing(CANARY_TMUX) {
        None => {}
        Some((listed, false)) if listed == record.execution_nonce => {}
        Some(_) => return Err("listing_differs"),
    }
    if !hold_released(&record.execution_nonce) {
        return Err("retired_hold");
    }
    if !o_bootstrap_base(channel, RuntimeHandoffKind::CodexTui) {
        return Err("o_not_seeded");
    }
    Ok(())
}

/// The O writer's refusal of a fresh placement, installed with the process's writer readiness.
static WRITER_REFUSAL: std::sync::OnceLock<fn(u64) -> Option<&'static str>> =
    std::sync::OnceLock::new();

/// Installs the writer's own judgement; the first install wins.
pub(crate) fn install_writer_refusal(judge: fn(u64) -> Option<&'static str>) {
    let _ = WRITER_REFUSAL.set(judge);
}

/// Before the writer readiness exists no channel has a live writer, as `host_missing` says.
fn writer_refusal(channel: u64) -> Option<&'static str> {
    WRITER_REFUSAL
        .get()
        .map_or(Some("host_missing"), |judge| judge(channel))
}

/// The owner a turn under `session_key` would write, as the Codex Herdr turn builds it.
fn expected_owner(
    provider: &ProviderKind,
    channel: u64,
    session_key: Option<&str>,
) -> Option<HostedOwner> {
    let identity = crate::domain::session_identity::SessionIdentity::parse(session_key?)?;
    Some(HostedOwner {
        provider: provider.as_str().into(),
        discord_token_hash: identity.token_hash?,
        channel_id: channel.to_string(),
        logical_key: identity.tmux_name,
        owner_node: session_hosts::local_node()?,
        runtime_root: crate::config::runtime_root()?.display().to_string(),
    })
}

#[cfg(unix)]
fn hold_released(nonce: &str) -> bool {
    crate::services::claude::herdr_turn::input_held(nonce) == Ok(false)
}

#[cfg(not(unix))]
fn hold_released(_nonce: &str) -> bool {
    false
}

/// Whether a front held on `channel` is watched for the canary bootstrap.
pub(crate) fn bootstrap_watched(provider: &ProviderKind, channel: u64) -> bool {
    *provider == ProviderKind::Codex
        && crate::services::herdr_launch::herdr_canary(channel)
        && herdr_turn_switched_on_for(provider)
}

/// One read of a held canary front's cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeldLook {
    /// The turn host would now plan the turn, or the channel left the bootstrap path.
    Ready,
    /// The writer is not ready and no bootstrap may start.
    Wait,
    /// The row or the admission switch could not be read.
    Unread,
}

/// The turn host's own judgement for a held front; the row is not read while the pin, the vacant
/// pane or the live actor already refuse.
pub(crate) async fn held_look(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    channel: u64,
    session_key: Option<&str>,
) -> HeldLook {
    if !bootstrap_watched(provider, channel) {
        return HeldLook::Ready;
    }
    if !o_writer_ready(channel, RuntimeHandoffKind::CodexTui)
        && (herdr_bootstrap::local_eligible(channel).is_err() || writer_refusal(channel).is_some())
    {
        return HeldLook::Wait;
    }
    match for_turn(pool, provider, channel, session_key).await {
        TurnHost::Refused(HerdrRefusal::OWriterNotReady) => HeldLook::Wait,
        TurnHost::Refused(
            HerdrRefusal::HostedRowUnreadable { .. } | HerdrRefusal::AdmissionStopped { .. },
        ) => HeldLook::Unread,
        _ => HeldLook::Ready,
    }
}

/// The launch endpoint a channel's configured Herdr endpoint names.
pub(crate) fn launch_endpoint(endpoint: ChannelEndpoint) -> HerdrLaunchEndpoint {
    HerdrLaunchEndpoint {
        execution_node: endpoint.execution_node,
        config_key: endpoint.key,
        socket_addr: endpoint.socket_path.display().to_string(),
        herdr_session: endpoint.herdr_session,
    }
}

#[cfg(test)]
#[path = "turn_host_tests.rs"]
mod tests;
