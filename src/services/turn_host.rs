//! Which host runs a provider turn, judged once before spawn. A channel configured for Herdr, or
//! one whose row still holds a Herdr execution, is never handed to tmux or the process driver.

use sqlx::PgPool;

use crate::config::session_hosts::{self, ChannelEndpoint};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, HostedObservation, HostedRecord, HostedState,
    load_hosted_execution_pg,
};
use crate::services::herdr_admission::{self, StopCause};
use crate::services::herdr_launch::{HerdrLaunchEndpoint, o_writer_ready};
use crate::services::provider::ProviderKind;

#[derive(Debug)]
pub(crate) enum TurnHost {
    /// The existing path, unchanged.
    Tmux,
    Herdr(Box<HerdrTurnPlan>),
    Refused(HerdrRefusal),
}

#[derive(Debug)]
pub(crate) struct HerdrTurnPlan {
    pub endpoint: HerdrLaunchEndpoint,
    /// `None` while the channel has no sessions row yet; read by the executor once one is wired.
    #[allow(dead_code)]
    pub row: Option<HostedObservation>,
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
    let Some(endpoint) = session_hosts::herdr_endpoint(channel_id) else {
        // An unconfigured channel's unreadable row keeps the existing path; a Herdr row refuses.
        return match herdr_trace(&read_row(pool, session_key).await) {
            Some(state) => TurnHost::Refused(HerdrRefusal::HostedHerdrUnconfigured { state }),
            None => TurnHost::Tmux,
        };
    };
    match configured_turn(pool, provider, channel_id, session_key, endpoint).await {
        Ok(plan) => TurnHost::Herdr(Box::new(plan)),
        Err(refusal) => TurnHost::Refused(refusal),
    }
}

/// The turn's first judgement, before it resets, reconciles or clears anything. An unconfigured
/// channel passes without I/O; a configured one is refused until a Herdr executor is wired.
pub(crate) async fn refusal_before_turn<F>(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    channel_id: u64,
    session_key: impl FnOnce() -> F,
) -> Option<HerdrRefusal>
where
    F: std::future::Future<Output = Option<String>>,
{
    session_hosts::herdr_endpoint(channel_id)?;
    let session_key = session_key().await;
    match for_turn(pool, provider, channel_id, session_key.as_deref()).await {
        TurnHost::Tmux => None,
        TurnHost::Refused(refusal) => Some(refusal),
        TurnHost::Herdr(_) => Some(HerdrRefusal::ExecutorNotWired),
    }
}

async fn configured_turn(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    channel_id: u64,
    session_key: Option<&str>,
    endpoint: ChannelEndpoint,
) -> Result<HerdrTurnPlan, HerdrRefusal> {
    if *provider != ProviderKind::Claude {
        let provider = provider.as_str().to_owned();
        return Err(HerdrRefusal::ProviderUnsupported { provider });
    }
    if session_hosts::local_node().as_deref() != Some(endpoint.execution_node.as_str()) {
        let node = endpoint.execution_node;
        return Err(HerdrRefusal::EndpointNotLocal { node });
    }
    let read = read_row(pool, session_key).await;
    if herdr_trace(&read) == Some(None) {
        let detail = "hosted execution record is unreadable".into();
        return Err(HerdrRefusal::HostedRowUnreadable { detail });
    }
    let row = read.map_err(|detail| HerdrRefusal::HostedRowUnreadable { detail })?;
    herdr_admission::check().map_err(|cause| HerdrRefusal::AdmissionStopped { cause })?;
    if !o_writer_ready(channel_id) {
        return Err(HerdrRefusal::OWriterNotReady);
    }
    let endpoint = HerdrLaunchEndpoint {
        execution_node: endpoint.execution_node,
        config_key: endpoint.key,
        socket_addr: endpoint.socket_path.display().to_string(),
        herdr_session: endpoint.herdr_session,
    };
    Ok(HerdrTurnPlan { endpoint, row })
}

#[cfg(test)]
#[path = "turn_host_tests.rs"]
mod tests;
