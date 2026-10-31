//! Turn mode for a Claude channel O took over before its first output: confirmed once the taken
//! execution ended and Legacy holds no unfinished turn, by a task that owns the retries.

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use sqlx::PgPool;

use super::codex::{Confirmation, ended, unfinished};
use super::{Retirement, retire_channel};
use crate::services::discord::tui_direct_pending_start::channel_lock;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding, SourceId};
use crate::services::tui_o::store::{InitOrigin, OStore};
use crate::services::tui_o::turn_mode::{self, TurnConfig};
use crate::services::tui_o::writer::binding::{
    BindingEvents, BindingLog, BindingRecord, BindingTarget,
};

/// The source a Claude takeover's init committed; None for an init of another origin.
pub(in crate::services::discord) fn takeover_source(
    channel: u64,
) -> Result<Option<SourceBinding>, String> {
    let store = crate::config::runtime_root().and_then(|root| OStore::existing(&root));
    let Some(store) = store else {
        return Ok(None);
    };
    let origin = store.read_init_origin(channel);
    if origin.map_err(|error| format!("{error:?}"))? != InitOrigin::LateClaudeTakeover {
        return Ok(None);
    }
    let init = store.read_init(channel);
    let sources = init.map_err(|error| format!("{error:?}"))?;
    let sources = sources.map(|init| init.sources).unwrap_or_default();
    let [source] = sources.as_slice() else {
        return Err(format!("the takeover init holds {} sources", sources.len()));
    };
    Ok(Some(SourceBinding {
        channel_id: channel,
        provider: ShadowProvider::Claude,
        source: source.source_id.clone(),
    }))
}

/// A takeover only took a transcript bound once, so its first bind names the taken execution even
/// after a resume binds the same transcript under another nonce.
fn taken_nonce(channel: u64, source: &SourceId) -> Result<String, String> {
    let events = BindingLog.binding_events_since(channel, 0)?;
    let same = |bound: &SourceId| {
        (bound.path == source.path && bound.ino == source.ino)
            && bound.session_id == source.session_id
    };
    let first = events.into_iter().find(|event| match &event.record {
        BindingRecord::Bound {
            new: BindingTarget::Source(bound),
            ..
        }
        | BindingRecord::Resolved { source: bound, .. } => same(bound),
        _ => false,
    });
    first
        .map(|event| event.execution_nonce)
        .ok_or_else(|| "no bind of the taken transcript".into())
}

/// A clear that confirmed the end of `nonce` while its pane wrote `source`.
fn cleared(channel: u64, source: &SourceId, nonce: &str) -> bool {
    let root = crate::config::runtime_root();
    let ends = root.map(|root| crate::services::tui_o::store::clear_end::read(&root, channel));
    ends.and_then(Result::ok).is_some_and(|ends| {
        (ends.iter()).any(|end| end.execution_nonce == nonce && end.source == *source)
    })
}

/// Confirms a selected takeover channel once the taken execution ended — its transcript Idle, a
/// confirmed clear of it, or `herdr_retired` naming it — and Legacy holds no unfinished turn.
pub(in crate::services::discord) fn confirm_takeover(
    channel: u64,
    config: &TurnConfig,
    takeover: &SourceBinding,
    herdr_retired: impl FnOnce(&str) -> bool,
) -> Confirmation {
    if !config.selects(channel) {
        return Confirmation::Unselected;
    }
    let provider = ProviderKind::Claude;
    let lock = channel_lock(provider.as_str(), channel);
    let Ok(_claims) = lock.try_lock() else {
        return Confirmation::Retry("a start claim holds the channel".into());
    };
    if turn_mode::transcript_turns(channel) {
        return Confirmation::Confirmed;
    }
    if let Some(detail) = unfinished(&provider, channel) {
        return Confirmation::Retry(detail);
    }
    let nonce = match taken_nonce(channel, &takeover.source) {
        Ok(nonce) => nonce,
        Err(detail) => return Confirmation::Retry(detail),
    };
    let source = &takeover.source;
    if ended(takeover).is_err() && !cleared(channel, source, &nonce) && !herdr_retired(&nonce) {
        return Confirmation::Retry("the taken execution has not ended".into());
    }
    match retire_channel(&provider, channel) {
        Ok(Retirement {
            removed,
            retry_pending: false,
        }) => {
            turn_mode::confirm(channel);
            tracing::info!(
                channel,
                removed,
                "[tui_o] Claude turn mode confirmed after its takeover"
            );
            Confirmation::Confirmed
        }
        outcome => Confirmation::Retry(format!("retirement left {outcome:?}")),
    }
}

/// The Herdr executions of `channel` this node's store records Retired.
async fn herdr_retired(pool: &PgPool, channel: u64) -> Vec<String> {
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedRecord, list_local_herdr_rows_pg,
    };
    let Some(node) = crate::config::session_hosts::local_node() else {
        return Vec::new();
    };
    let rows = list_local_herdr_rows_pg(pool, &node, &["retired"]).await;
    let channel = channel.to_string();
    let rows = rows.unwrap_or_default().into_iter();
    rows.filter_map(|row| match row.record {
        HostedRecord::Known(record) if record.owner.channel_id == channel => {
            Some(record.execution_nonce)
        }
        _ => None,
    })
    .collect()
}

/// Channels whose confirmation task runs in this process.
static RUNNING: Mutex<BTreeSet<u64>> = Mutex::new(BTreeSet::new());

struct Running(u64);

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// Owns a takeover channel's confirmation: retried on each bind and every few seconds until it is
/// confirmed or unselected, then the channel joins the turn supervisor.
pub(in crate::services::discord) fn spawn_confirm(channel: u64, config: TurnConfig) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!(
            channel,
            "[tui_o] no runtime owns the Claude turn mode retries"
        );
        return;
    };
    if !RUNNING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(channel)
    {
        return;
    }
    let pool = crate::db::cancel_tombstones::global_pool().cloned();
    runtime.spawn(async move {
        let _running = Running(channel);
        let mut binds =
            crate::services::tui_prompt_dedupe::binding_events::subscribe_binding_events(channel)
                .ok();
        let mut logged = None;
        loop {
            let retired = match &pool {
                Some(pool) => herdr_retired(pool, channel).await,
                None => Vec::new(),
            };
            let confirmed = match takeover_source(channel) {
                Ok(Some(takeover)) => confirm_takeover(channel, &config, &takeover, |nonce| {
                    retired.iter().any(|retired| retired == nonce)
                }),
                Ok(None) => Confirmation::Unselected,
                Err(detail) => Confirmation::Retry(detail),
            };
            match confirmed {
                Confirmation::Confirmed => {
                    let provider = ProviderKind::Claude;
                    crate::services::discord::turn_presence::supervisor::register(
                        &provider,
                        &[channel],
                    );
                    return;
                }
                Confirmation::Unselected => return,
                Confirmation::Retry(detail) if logged.as_ref() != Some(&detail) => {
                    tracing::info!(channel, detail, "[tui_o] Claude turn mode waits");
                    logged = Some(detail);
                }
                Confirmation::Retry(_) => {}
            }
            let wait = tokio::time::sleep(Duration::from_secs(5));
            let closed = match &mut binds {
                Some(binds) => tokio::select! {
                    changed = binds.changed() => changed.is_err(),
                    () = wait => false,
                },
                None => {
                    wait.await;
                    false
                }
            };
            if closed {
                binds = None;
            }
        }
    });
}

#[cfg(test)]
#[path = "claude_tests.rs"]
mod tests;
