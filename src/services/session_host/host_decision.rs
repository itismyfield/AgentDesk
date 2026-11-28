//! The host a channel's command, cleanup or input acts on now: a channel's own boot entry as before,
//! a thread under a Herdr parent by its own row and tmux session read now, never by a cache.

use sqlx::PgPool;

use super::thread_host::{self, PolicyHost};
use crate::config::session_hosts::ChannelEndpoint;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind, resolve_session_row_pg,
};
use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
use crate::services::platform::tmux::SessionPresence;
use crate::services::provider::ProviderKind;
use crate::services::session_host::{
    HostPresence, HostSessionRef, InteractiveSessionHost, TmuxHost,
};
use crate::services::turn_host;

/// Why no host is decided; every consumer leaves both hosts untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldReason {
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
pub enum HostDecision {
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
    pub fn tmux_refusal(&self) -> Option<String> {
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

    pub fn is_herdr(&self) -> bool {
        matches!(self, Self::Herdr { .. })
    }
}

/// The decision for `channel`. Only a thread under a Herdr parent reads its row and tmux session.
pub async fn host_decision(
    pool: Option<&PgPool>,
    token_hash: Option<&str>,
    provider: Option<&ProviderKind>,
    channel: u64,
) -> HostDecision {
    decide(pool, Source::Token(token_hash), provider, channel, true).await
}

/// The decision for a caller holding `channel`'s session key: its row is read by that key, a
/// legacy `host:tmux` key included, and a missing provider is read from the key.
pub async fn host_decision_for_key(
    pool: Option<&PgPool>,
    session_key: &str,
    provider: Option<&ProviderKind>,
    channel: u64,
) -> HostDecision {
    let derived = provider.cloned().or_else(|| provider_of_key(session_key));
    decide(
        pool,
        Source::Key(session_key),
        derived.as_ref(),
        channel,
        true,
    )
    .await
}

/// The decision for a slot thread from every row it has (`channel_id` or `thread_channel_id`).
/// No row at all, read without error, is no execution; any row decides by its own key.
pub async fn host_decision_for_rows(pool: &PgPool, channel: u64) -> HostDecision {
    if let Err(decided) = thread_endpoint(channel) {
        return decided;
    }
    let rows: Result<Vec<(Option<String>, Option<String>)>, _> = sqlx::query_as(
        "SELECT provider, session_key FROM sessions
         WHERE channel_id = $1 OR thread_channel_id = $1",
    )
    .bind(channel.to_string())
    .fetch_all(pool)
    .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => return HostDecision::Hold(HoldReason::Execution(format!("rows: {error}"))),
    };
    let mut decided = HostDecision::Tmux;
    for (provider, key) in rows {
        let Some(key) = key.filter(|key| !key.trim().is_empty()) else {
            return HostDecision::Hold(HoldReason::Execution("row without session key".into()));
        };
        let provider = provider.as_deref().and_then(ProviderKind::from_str);
        match host_decision_for_key(Some(pool), &key, provider.as_ref(), channel).await {
            hold @ HostDecision::Hold(_) => return hold,
            herdr @ HostDecision::Herdr { .. } => decided = herdr,
            HostDecision::Tmux => {}
        }
    }
    decided
}

/// The provider a session key names, else the one its tmux name carries.
fn provider_of_key(session_key: &str) -> Option<ProviderKind> {
    let identity = crate::domain::session_identity::SessionIdentity::parse(session_key)?;
    if let Some(provider) = identity
        .provider_from_key
        .as_deref()
        .and_then(ProviderKind::from_str)
    {
        return Some(provider);
    }
    let parsed = crate::services::provider::parse_provider_and_channel_from_tmux_name;
    parsed(&identity.tmux_name).map(|(provider, _)| provider)
}

/// Whether an input of `channel` runs on Herdr now: the same decision, without the tmux probe
/// that tells tmux from no execution, since neither runs on Herdr.
pub async fn input_runs_herdr(
    pool: Option<&PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    channel: u64,
) -> bool {
    let source = Source::Token(Some(token_hash));
    decide(pool, source, Some(provider), channel, false)
        .await
        .is_herdr()
}

/// How a thread's row is found: by its canonical identity under a token, or by a key in hand.
#[derive(Clone, Copy)]
enum Source<'a> {
    Token(Option<&'a str>),
    Key(&'a str),
}

/// The parent endpoint of a thread under a Herdr parent; any other channel is decided here.
fn thread_endpoint(channel: u64) -> Result<ChannelEndpoint, HostDecision> {
    match thread_host::parent_policy(channel) {
        PolicyHost::Herdr {
            endpoint,
            via_parent: None,
        } => Err(HostDecision::herdr(endpoint)),
        PolicyHost::Tmux => Err(HostDecision::Tmux),
        PolicyHost::Unknown(_) => Err(HostDecision::Hold(HoldReason::RelationUnresolved)),
        PolicyHost::Herdr { endpoint, .. } => Ok(endpoint),
    }
}

async fn decide(
    pool: Option<&PgPool>,
    source: Source<'_>,
    provider: Option<&ProviderKind>,
    channel: u64,
    probe_tmux: bool,
) -> HostDecision {
    let endpoint = match thread_endpoint(channel) {
        Ok(endpoint) => endpoint,
        Err(decided) => return decided,
    };
    let Some(provider) = provider else {
        return HostDecision::Hold(HoldReason::ProviderUnknown);
    };
    let observed = observe_thread(pool, source, provider, channel, &endpoint, probe_tmux);
    match observed.await {
        Observed::Herdr => HostDecision::herdr(endpoint),
        Observed::Tmux => HostDecision::Tmux,
        // Nothing runs: before activation the next execution of a thread is tmux, as
        // `thread_host::next_host` answers for any thread without a Herdr execution.
        Observed::Nothing => HostDecision::Tmux,
        Observed::Unknown(detail) => HostDecision::Hold(HoldReason::Execution(detail)),
    }
}

/// The key of `channel`'s canonical row under `token_hash`, if it has one.
async fn canonical_key(
    pool: &PgPool,
    token_hash: &str,
    provider: &ProviderKind,
    channel: u64,
) -> Result<Option<String>, String> {
    let channel_text = channel.to_string();
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: token_hash,
        channel_id: &channel_text,
    };
    resolve_session_row_pg(pool, None, Some(provider.as_str()), Some(identity))
        .await
        .map(|row| row.map(|(_, key)| key))
        .map_err(|error| format!("session row: {error:?}"))
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
    source: Source<'_>,
    provider: &ProviderKind,
    channel: u64,
    endpoint: &ChannelEndpoint,
    probe_tmux: bool,
) -> Observed {
    let Some(pool) = pool else {
        return Observed::Unknown("no database".into());
    };
    let key = match source {
        Source::Key(key) => key.to_owned(),
        Source::Token(None) => return Observed::Unknown("no token".into()),
        Source::Token(Some(token_hash)) => {
            match canonical_key(pool, token_hash, provider, channel).await {
                Ok(Some(key)) => key,
                Ok(None) => return Observed::Nothing,
                Err(detail) => return Observed::Unknown(detail),
            }
        }
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

#[cfg(any(test, feature = "test-support"))]
static FORCED_PRESENCE: std::sync::Mutex<
    std::collections::BTreeMap<String, (SessionPresence, usize)>,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());

fn presence(name: &str) -> SessionPresence {
    #[cfg(any(test, feature = "test-support"))]
    if let Some((forced, probes)) = FORCED_PRESENCE.lock().unwrap().get_mut(name) {
        *probes += 1;
        return *forced;
    }
    match TmuxHost.presence(HostSessionRef::tmux(name)) {
        HostPresence::Present => SessionPresence::Present,
        HostPresence::Missing => SessionPresence::Missing,
        HostPresence::ProbeFailed => SessionPresence::ProbeFailed,
    }
}

/// Answers the tmux probe of `name` with `presence` for this test.
#[cfg(any(test, feature = "test-support"))]
pub fn force_presence_for_test(name: &str, presence: SessionPresence) {
    FORCED_PRESENCE
        .lock()
        .unwrap()
        .insert(name.to_owned(), (presence, 0));
}

/// How many forced probes of `name` ran.
#[cfg(any(test, feature = "test-support"))]
pub fn forced_probes(name: &str) -> usize {
    FORCED_PRESENCE
        .lock()
        .unwrap()
        .get(name)
        .map_or(0, |entry| entry.1)
}

#[cfg(test)]
#[path = "host_decision_tests.rs"]
mod tests;
