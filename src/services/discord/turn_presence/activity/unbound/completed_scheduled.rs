//! Exact completed reservations resolve orphan pane ownership without granting cleanup authority.
use super::super::routine_owner::RoutineEvidence;
use super::*;
use crate::services::provider::parse_provider_and_channel_from_tmux_name;

#[derive(Clone, Debug)]
pub(super) struct Proof {
    pub(super) name: String,
    pub(super) channel: u64,
    provider: ProviderKind,
    pub(super) binding: BindingEvent,
    other_inflight: Option<String>,
    seq: u64,
    sequence: tokio::sync::watch::Receiver<u64>,
}

impl PartialEq for Proof {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.channel == other.channel
            && self.provider == other.provider
            && self.binding == other.binding
            && self.other_inflight == other.other_inflight
            && self.seq == other.seq
    }
}
impl Eq for Proof {}

fn latest(
    channel: u64,
    provider: &ProviderKind,
    name: &str,
) -> Result<Option<BindingEvent>, String> {
    let shadow = match provider {
        ProviderKind::Claude => ShadowProvider::Claude,
        ProviderKind::Codex => ShadowProvider::Codex,
        _ => return Err("unsupported scheduled provider".into()),
    };
    Ok(ChannelBindingLog::new(channel, shadow)
        .binding_events_since(channel, 0)?
        .into_iter()
        .rev()
        .find(|event| event.tmux_session == name))
}

fn other_inflight(
    channel: u64,
    provider: &ProviderKind,
    name: &str,
) -> Result<Option<String>, String> {
    match inflight::load_inflight_state_read_only_result(provider, channel)? {
        None => Ok(None),
        Some(row)
            if row.channel_id == channel
                && row.provider == provider.as_str()
                && !row.runtime_kind_unknown_on_disk =>
        {
            row.tmux_session_name
                .filter(|current| !current.trim().is_empty() && current != name)
                .map(Some)
                .ok_or_else(|| "scheduled execution may be inflight".into())
        }
        _ => Err("unresolved scheduled inflight".into()),
    }
}

/// Claude logs its launch binding before the session id is known and never re-logs it once the
/// id fills in, so an unnamed source is that session's only when its transcript is named after it.
fn names_native(source: &crate::services::tui_o::shadow::SourceId, native: &str) -> bool {
    source.session_id == native
        || (source.session_id.is_empty()
            && source.path.file_stem().and_then(|stem| stem.to_str()) == Some(native))
}

impl Proof {
    pub(super) fn sequence_current(&self) -> bool {
        *self.sequence.borrow() == self.seq
    }

    pub(super) fn current(&self) -> bool {
        self.sequence_current()
            && latest(self.channel, &self.provider, &self.name)
                .ok()
                .flatten()
                .as_ref()
                == Some(&self.binding)
            && other_inflight(self.channel, &self.provider, &self.name).as_ref()
                == Ok(&self.other_inflight)
    }
}

#[derive(sqlx::FromRow)]
struct Outcome {
    session_key: String,
    definition: String,
    channel: String,
    provider: String,
    native: String,
    launched: chrono::DateTime<chrono::Utc>,
    finished: chrono::DateTime<chrono::Utc>,
}

pub(super) async fn collect(
    pool: &sqlx::PgPool,
    routines: &Result<RoutineEvidence, String>,
    ports: Arc<LivePorts>,
) -> Result<Vec<Proof>, String> {
    let routines = routines.as_ref().map_err(Clone::clone)?;
    let keys: BTreeMap<_, _> = routines
        .keys
        .iter()
        .filter(|(_, receipt)| {
            parse_provider_and_channel_from_tmux_name(&receipt.name).is_some_and(|(provider, _)| {
                receipt.name.starts_with(&format!(
                    "{}-{}-scheduled-smsg_",
                    crate::services::provider::TMUX_SESSION_PREFIX,
                    provider.as_str()
                ))
            })
        })
        .map(|(key, receipt)| (key.clone(), receipt.name.clone()))
        .collect();
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<Outcome> = sqlx::query_as(
        "SELECT t.session_key, m.id AS definition, t.channel_id AS channel,
                t.provider, t.session_id AS native, d.launch_committed_at AS launched,
                d.turn_outcome_at AS finished
         FROM scheduled_messages m
         JOIN scheduled_message_deliveries d ON d.scheduled_message_id = m.id
         JOIN turns t ON t.turn_id = d.turn_id
         WHERE m.context_strategy = 'snapshot' AND d.delivery_kind = 'agent'
           AND t.session_key = ANY($1) AND t.session_id IS NOT NULL AND t.provider IS NOT NULL
           AND d.launch_committed_at IS NOT NULL AND d.turn_outcome_at IS NOT NULL
           AND d.turn_outcome IN ('delivered', 'no_reply', 'provider_error', 'empty_response')
           AND NOT EXISTS (
               SELECT 1 FROM scheduled_message_deliveries active
               WHERE active.scheduled_message_id = m.id AND active.status = 'running'
                 AND active.launch_committed_at IS NOT NULL AND active.turn_outcome IS NULL
           )",
    )
    .bind(keys.keys().cloned().collect::<Vec<_>>())
    .fetch_all(pool)
    .await
    .map_err(|error| format!("scheduled ownership read: {error}"))?;
    tokio::task::spawn_blocking(move || {
        ports.scoped(|| {
            let mut proofs: BTreeMap<String, Option<Proof>> = BTreeMap::new();
            let mut bindings = BTreeMap::new();
            for row in rows {
                let Some(name) = keys.get(&row.session_key) else {
                    continue;
                };
                let Some(provider) = ProviderKind::from_str(&row.provider) else {
                    continue;
                };
                let Ok(channel) = row.channel.parse::<u64>() else {
                    continue;
                };
                if channel == 0
                    || crate::config::session_hosts::herdr_endpoint(channel).is_some()
                    || provider.build_tmux_session_name(&format!("scheduled:{}", row.definition))
                        != *name
                {
                    continue;
                }
                let Some(binding) = bindings
                    .entry((channel, name.clone()))
                    .or_insert_with(|| latest(channel, &provider, name))
                    .as_ref()
                    .map_err(Clone::clone)?
                    .as_ref()
                else {
                    continue;
                };
                let source = match &binding.record {
                    BindingRecord::Bound {
                        new: crate::services::tui_o::writer::binding::BindingTarget::Source(source),
                        ..
                    }
                    | BindingRecord::Resolved { source, .. } => source,
                    _ => continue,
                };
                if !names_native(source, &row.native)
                    || row.native.trim().is_empty()
                    || binding.execution_nonce.trim().is_empty()
                    || binding.committed_at < row.launched
                    || binding.committed_at > row.finished
                {
                    continue;
                }
                let Ok(other_inflight) = other_inflight(channel, &provider, name) else {
                    continue;
                };
                let sequence =
                    crate::services::tui_prompt_dedupe::binding_events::subscribe_binding_events(
                        channel,
                    )
                    .map_err(|error| error.to_string())?;
                let seq = *sequence.borrow();
                let proof = Proof {
                    name: name.clone(),
                    channel,
                    provider,
                    binding: binding.clone(),
                    other_inflight,
                    seq,
                    sequence,
                };
                proofs
                    .entry(name.clone())
                    .and_modify(|old| {
                        if old.as_ref() != Some(&proof) {
                            *old = None;
                        }
                    })
                    .or_insert(Some(proof));
            }
            Ok(proofs.into_values().flatten().collect())
        })
    })
    .await
    .map_err(|error| error.to_string())?
}
