//! Which host runs a provider turn, judged once before spawn. A channel configured for Herdr, or
//! one whose row still holds a Herdr execution, is never handed to tmux or the process driver.

use sqlx::PgPool;

use crate::config::session_hosts::{self, HerdrEndpoint};
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
    Herdr(HerdrTurnPlan),
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

/// What the sessions row says about Herdr, read before any tmux or Herdr I/O.
enum RowRead {
    Read(Option<HostedObservation>),
    Failed(String),
}

impl RowRead {
    /// The Herdr execution the row still holds: Pending, Bound or an unreadable record.
    fn herdr_trace(&self) -> Option<Option<HostedState>> {
        let RowRead::Read(Some(observed)) = self else {
            return None;
        };
        match &observed.record {
            HostedRecord::Legacy => None,
            HostedRecord::Known(record) if record.state == HostedState::Retired => None,
            HostedRecord::Known(record) => Some(Some(record.state)),
            HostedRecord::Unknown(_) => Some(None),
        }
    }
}

async fn read_row(pool: Option<&PgPool>, session_key: Option<&str>) -> RowRead {
    let (Some(pool), Some(session_key)) = (pool, session_key) else {
        return RowRead::Failed("no database or session key".into());
    };
    match load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(session_key)).await {
        HostedLookup::Found(observed) => RowRead::Read(Some(observed)),
        HostedLookup::Missing => RowRead::Read(None),
        HostedLookup::Unknown(detail) => RowRead::Failed(detail),
        HostedLookup::Conflict(kind) => RowRead::Failed(format!("{kind:?}")),
    }
}

/// Call once per turn after the inflight save, before spawn. Admission and O readiness are read
/// only for a configured channel.
pub(crate) async fn for_turn(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    channel_id: u64,
    session_key: Option<&str>,
) -> TurnHost {
    let Some(endpoint) = session_hosts::herdr_endpoint(channel_id) else {
        // An unconfigured channel's unreadable row keeps the existing path; a Herdr row refuses.
        return match read_row(pool, session_key).await.herdr_trace() {
            Some(state) => TurnHost::Refused(HerdrRefusal::HostedHerdrUnconfigured { state }),
            None => TurnHost::Tmux,
        };
    };
    match configured_turn(pool, provider, channel_id, session_key, endpoint).await {
        Ok(plan) => TurnHost::Herdr(plan),
        Err(refusal) => TurnHost::Refused(refusal),
    }
}

async fn configured_turn(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    channel_id: u64,
    session_key: Option<&str>,
    endpoint: HerdrEndpoint,
) -> Result<HerdrTurnPlan, HerdrRefusal> {
    if *provider != ProviderKind::Claude {
        let provider = provider.as_str().to_owned();
        return Err(HerdrRefusal::ProviderUnsupported { provider });
    }
    if session_hosts::local_node().as_deref() != Some(endpoint.execution_node.as_str()) {
        let node = endpoint.execution_node;
        return Err(HerdrRefusal::EndpointNotLocal { node });
    }
    let row = match read_row(pool, session_key).await {
        RowRead::Failed(detail) => return Err(HerdrRefusal::HostedRowUnreadable { detail }),
        read @ RowRead::Read(_) if read.herdr_trace() == Some(None) => {
            let detail = "hosted execution record is unreadable".into();
            return Err(HerdrRefusal::HostedRowUnreadable { detail });
        }
        RowRead::Read(row) => row,
    };
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
