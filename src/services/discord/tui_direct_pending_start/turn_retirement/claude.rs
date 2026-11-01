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
use crate::services::tui_o::store::rotation::TakeoverEnd;
use crate::services::tui_o::store::spool::source_key;
use crate::services::tui_o::store::{InitOrigin, OStore};
use crate::services::tui_o::turn_mode::{self, TurnConfig};
use crate::services::tui_o::writer::binding::{
    BindingEvents, BindingLog, BindingRecord, BindingTarget,
};

/// The source a Claude takeover's init committed; None for an init of another origin.
fn store() -> Option<OStore> {
    crate::config::runtime_root().and_then(|root| OStore::existing(&root))
}

/// The same transcript file and session; a reboot may renumber the device.
fn same(bound: &SourceId, source: &SourceId) -> bool {
    (bound.path == source.path && bound.ino == source.ino) && bound.session_id == source.session_id
}

pub(in crate::services::discord) fn takeover_source(
    channel: u64,
) -> Result<Option<SourceBinding>, String> {
    let Some(store) = store() else {
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
    let first = events.into_iter().find(|event| match &event.record {
        BindingRecord::Bound {
            new: BindingTarget::Source(bound),
            ..
        }
        | BindingRecord::Resolved { source: bound, .. } => same(bound, source),
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

/// A hop O's rotation proved the taken transcript's session left.
fn hopped(channel: u64, source: &SourceId) -> bool {
    let rotation = store().map(|store| store.peek_rotation(channel));
    rotation.and_then(Result::ok).is_some_and(|rotation| {
        let next = rotation.successors.get(&source_key(source));
        next.is_some_and(|next| next.proof.is_some())
    })
}

fn kept_end(channel: u64, source: &SourceId, nonce: &str) -> bool {
    let end = store().and_then(|store| store.takeover_end(channel).ok().flatten());
    end.is_some_and(|end| same(&end.source, source) && end.nonce == nonce)
}

/// A failure to keep it only leaves the live evidence, read again on the next try.
fn keep_end(channel: u64, source: &SourceId, nonce: &str, basis: &str) {
    let end = TakeoverEnd {
        source: source.clone(),
        nonce: nonce.into(),
        basis: basis.into(),
    };
    if let Some(Err(error)) = store().map(|store| store.keep_takeover_end(channel, &end)) {
        tracing::warn!(
            channel,
            ?error,
            "[tui_o] a Claude takeover's end was not kept"
        );
    }
}

/// Keeps the end of execution `nonce` when it is the one a Claude takeover of `channel` took.
pub(crate) fn note_ended(channel: u64, nonce: &str, basis: &str) {
    let Ok(Some(takeover)) = takeover_source(channel) else {
        return;
    };
    if taken_nonce(channel, &takeover.source).is_ok_and(|taken| taken == nonce) {
        keep_end(channel, &takeover.source, nonce, basis);
    }
}

/// Confirms once the taken execution ended (Idle, a kept end, a clear, `herdr_retired` or a proven
/// hop) and Legacy holds no unfinished turn; a live end is kept so nothing later erases it.
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
    let nonce = match taken_nonce(channel, &takeover.source) {
        Ok(nonce) => nonce,
        Err(detail) => return Confirmation::Retry(detail),
    };
    let source = &takeover.source;
    let mut gone = kept_end(channel, source, &nonce);
    if !gone {
        let basis = if cleared(channel, source, &nonce) {
            Some("confirmed_clear")
        } else if herdr_retired(&nonce) {
            Some("herdr_retired")
        } else {
            hopped(channel, source).then_some("proven_hop")
        };
        if let Some(basis) = basis {
            keep_end(channel, source, &nonce, basis);
        }
        gone = basis.is_some() || ended(takeover).is_ok();
    }
    if let Some(detail) = unfinished(&provider, channel) {
        return Confirmation::Retry(detail);
    }
    if !gone {
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
    let pool = crate::db::cancel_tombstones::global_pool().cloned();
    spawn_confirm_with(channel, config, pool);
}

fn spawn_confirm_with(channel: u64, config: TurnConfig, pool: Option<PgPool>) {
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

/// Whether a selected takeover channel confirms now with no Herdr row read as retired.
#[cfg(test)]
pub(crate) fn confirmed_now(channel: u64) -> bool {
    let config = TurnConfig {
        channels: [channel].into(),
        all_owned: Some(false),
    };
    let Ok(Some(takeover)) = takeover_source(channel) else {
        return false;
    };
    confirm_takeover(channel, &config, &takeover, |_| false) == Confirmation::Confirmed
}

#[cfg(test)]
#[path = "claude_tests.rs"]
pub(crate) mod tests;
