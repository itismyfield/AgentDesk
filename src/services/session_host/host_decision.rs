//! The host a channel's command, cleanup or input acts on now. A channel's own boot entry decides
//! as it always did; a thread under a Herdr parent is decided by the execution observed for it now
//! (its own row, then its tmux session), never by the parent's policy alone or by a cache.

use sqlx::PgPool;

use super::thread_host::{self, PolicyHost};
use crate::config::session_hosts::ChannelEndpoint;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind, resolve_session_row_pg,
};
use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
use crate::services::platform::tmux::SessionPresence;
use crate::services::provider::ProviderKind;
use crate::services::turn_host;

/// Why no host is decided; every consumer leaves both hosts untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HoldReason {
    RelationUnresolved,
    ProviderUnknown,
    Execution(String),
}

impl std::fmt::Display for HoldReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RelationUnresolved => f.write_str("스레드 부모 미확인"),
            Self::ProviderUnknown => f.write_str("provider 미확인"),
            Self::Execution(detail) => write!(f, "현재 실행 미확인: {detail}"),
        }
    }
}

/// The host a channel's operation targets. It grants no execution; consumers match all three.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HostDecision {
    Tmux,
    Herdr {
        endpoint_key: String,
        execution_node: String,
    },
    Hold(HoldReason),
}

impl HostDecision {
    fn herdr(endpoint: ChannelEndpoint) -> Self {
        Self::Herdr {
            endpoint_key: endpoint.key,
            execution_node: endpoint.execution_node,
        }
    }

    /// For a consumer that leaves the tmux session as it is unless the decision is tmux.
    pub(crate) fn tmux_refusal(&self) -> Option<String> {
        match self {
            Self::Tmux => None,
            Self::Herdr { endpoint_key, .. } => Some(format!(
                "Herdr 설정 채널(endpoint `{endpoint_key}`)이라 tmux 세션을 바꾸지 않아요"
            )),
            Self::Hold(reason) => Some(format!(
                "실행 호스트를 확인하지 못해 tmux 세션을 바꾸지 않아요({reason})"
            )),
        }
    }

    pub(crate) fn is_herdr(&self) -> bool {
        matches!(self, Self::Herdr { .. })
    }
}

/// The token hash a full session key names, for a caller holding only the key.
pub(crate) fn token_hash_of(session_key: &str) -> Option<String> {
    crate::domain::session_identity::SessionIdentity::parse(session_key)?.token_hash
}

/// The decision for `channel`. Only a thread under a Herdr parent reads its row and tmux session.
pub(crate) async fn host_decision(
    pool: Option<&PgPool>,
    token_hash: Option<&str>,
    provider: Option<&ProviderKind>,
    channel: u64,
) -> HostDecision {
    decide(pool, token_hash, provider, channel, true).await
}

/// Whether an input of `channel` runs on Herdr now: the same decision, without the tmux probe
/// that tells tmux from no execution, since neither runs on Herdr.
pub(crate) async fn input_runs_herdr(
    pool: Option<&PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    channel: u64,
) -> bool {
    decide(pool, Some(token_hash), Some(provider), channel, false)
        .await
        .is_herdr()
}

async fn decide(
    pool: Option<&PgPool>,
    token_hash: Option<&str>,
    provider: Option<&ProviderKind>,
    channel: u64,
    probe_tmux: bool,
) -> HostDecision {
    let endpoint = match thread_host::parent_policy(channel) {
        PolicyHost::Herdr {
            endpoint,
            via_parent: None,
        } => return HostDecision::herdr(endpoint),
        PolicyHost::Tmux => return HostDecision::Tmux,
        PolicyHost::Unknown(_) => return HostDecision::Hold(HoldReason::RelationUnresolved),
        PolicyHost::Herdr { endpoint, .. } => endpoint,
    };
    let Some(provider) = provider else {
        return HostDecision::Hold(HoldReason::ProviderUnknown);
    };
    let observed = observe_thread(pool, token_hash, provider, channel, &endpoint, probe_tmux);
    match observed.await {
        Observed::Herdr => HostDecision::herdr(endpoint),
        Observed::Tmux => HostDecision::Tmux,
        // Nothing runs: before activation the next execution of a thread is tmux, as
        // `thread_host::next_host` answers for any thread without a Herdr execution.
        Observed::Nothing => HostDecision::Tmux,
        Observed::Unknown(detail) => HostDecision::Hold(HoldReason::Execution(detail)),
    }
}

/// A thread's execution now, from its own row and its tmux session.
#[derive(Debug, PartialEq, Eq)]
enum Observed {
    /// Its Bound row matches its owner and its parent's endpoint.
    Herdr,
    Tmux,
    /// No row, a legacy or retired row with no tmux session.
    Nothing,
    Unknown(String),
}

async fn observe_thread(
    pool: Option<&PgPool>,
    token_hash: Option<&str>,
    provider: &ProviderKind,
    channel: u64,
    endpoint: &ChannelEndpoint,
    probe_tmux: bool,
) -> Observed {
    let (Some(pool), Some(token_hash)) = (pool, token_hash) else {
        return Observed::Unknown("no database or token".into());
    };
    let channel_text = channel.to_string();
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: token_hash,
        channel_id: &channel_text,
    };
    let resolved = resolve_session_row_pg(pool, None, Some(provider.as_str()), Some(identity));
    let key = match resolved.await {
        Ok(Some((_, key))) => key,
        Ok(None) => return Observed::Nothing,
        Err(error) => return Observed::Unknown(format!("session row: {error:?}")),
    };
    let row = turn_host::read_row(Some(pool), Some(&key)).await;
    match &row {
        Err(detail) => return Observed::Unknown(detail.clone()),
        Ok(Some(observed)) => match &observed.record {
            HostedRecord::Known(record)
                if matches!(record.state, HostedState::Pending | HostedState::Bound) =>
            {
                let live = turn_host::is_live_thread_endpoint;
                return if live(channel, provider, Some(&key), &row, endpoint) {
                    Observed::Herdr
                } else {
                    Observed::Unknown("hosted row is not this thread's pane".into())
                };
            }
            HostedRecord::Unknown(_) => {
                return Observed::Unknown("hosted execution record is unreadable".into());
            }
            HostedRecord::Known(_) | HostedRecord::Legacy => {}
        },
        Ok(None) => {}
    }
    if !probe_tmux {
        return Observed::Nothing;
    }
    let Some(name) = crate::domain::session_identity::tmux_name_from_session_key(&key) else {
        return Observed::Unknown("session key names no tmux session".into());
    };
    match presence(&name) {
        SessionPresence::Present => Observed::Tmux,
        SessionPresence::Missing => Observed::Nothing,
        SessionPresence::ProbeFailed => Observed::Unknown("tmux probe failed".into()),
    }
}

#[cfg(test)]
static FORCED_PRESENCE: std::sync::Mutex<
    std::collections::BTreeMap<String, (SessionPresence, usize)>,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());

fn presence(name: &str) -> SessionPresence {
    #[cfg(test)]
    if let Some((forced, probes)) = FORCED_PRESENCE.lock().unwrap().get_mut(name) {
        *probes += 1;
        return *forced;
    }
    crate::services::platform::tmux::session_presence(name)
}

/// Answers the tmux probe of `name` with `presence` for this test.
#[cfg(test)]
pub(crate) fn force_presence_for_test(name: &str, presence: SessionPresence) {
    FORCED_PRESENCE
        .lock()
        .unwrap()
        .insert(name.to_owned(), (presence, 0));
}

/// How many forced probes of `name` ran.
#[cfg(test)]
pub(crate) fn forced_probes(name: &str) -> usize {
    FORCED_PRESENCE.lock().unwrap().get(name).map_or(0, |entry| entry.1)
}

#[cfg(test)]
#[path = "host_decision_tests.rs"]
mod tests;
