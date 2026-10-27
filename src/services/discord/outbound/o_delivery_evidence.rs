//! Exact message readback over the union of local Posted and existing shared receipts.

use super::{
    o_delivery_manifest::{self as manifest, Manifest},
    o_writer_io::GatewayPort,
};
use crate::services::tui_o::{
    repost::{o_piece_delivery::PieceKey, projection::delivery_candidates},
    store::{delivery_evidence, ledger::PieceOutcome},
    writer::{DiscordPort, confirm},
};
use crate::services::{discord::SharedData, provider::ProviderKind};
use futures::StreamExt;
use poise::serenity_prelude::{self as serenity, ChannelId, MessageId};
use std::{path::Path, sync::Arc};
use tokio::time::Instant;

#[derive(Debug)]
pub(in crate::services::discord) struct TailCovered {
    _private: (),
}

pub(super) struct EpisodeCovered {
    pub posted_ms: u64,
}

async fn verify<P: DiscordPort>(
    pool: Option<&sqlx::PgPool>,
    port: &P,
    plan: &Manifest,
    sent: usize,
    deadline: Instant,
    local: Option<&crate::services::tui_o::store::ledger::LedgerState>,
) -> Option<u64> {
    if plan.version != crate::services::tui_o::shadow::IDENTITY_VERSION
        || !plan.body.is_char_boundary(sent)
        || sent >= plan.body.len()
    {
        return None;
    }
    let channel = plan.pieces.first()?.key.channel_id;

    let required: Vec<_> = plan
        .pieces
        .iter()
        .filter(|piece| piece.body_end > sent)
        .cloned()
        .collect();
    if required.is_empty() {
        return None;
    }
    let checks = required.into_iter().map(|piece| async move {
        let mut ids = Vec::new();
        if let Some(local) = &local
            && let Some((_, record)) = local.latest_piece(&piece.key, piece.index)
            && record.payload == piece.payload
            && let Some(PieceOutcome::Posted(id)) = record.outcome
        {
            ids.push(id);
        }
        for &id in &ids {
            if let Some(time) = exact(port, channel, id, &piece.payload, deadline).await {
                return Some(time);
            }
        }
        ids.clear();
        if let (Some(pool), Some(key)) = (pool, PieceKey::new(piece.key.clone(), piece.index))
            && let Ok(Ok(shared)) = tokio::time::timeout_at(
                deadline,
                delivery_candidates(pool, &key, port.bot_id(), &piece.payload),
            )
            .await
        {
            ids.extend(shared);
        }
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            if Instant::now() >= deadline {
                return None;
            }
            if let Some(time) = exact(port, channel, id, &piece.payload, deadline).await {
                return Some(time);
            }
        }
        None
    });
    let results: Vec<_> = futures::stream::iter(checks)
        .buffer_unordered(8)
        .collect()
        .await;
    results
        .into_iter()
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .max()
}

async fn exact<P: DiscordPort>(
    port: &P,
    channel: u64,
    id: u64,
    payload: &str,
    deadline: Instant,
) -> Option<u64> {
    if id == 0 {
        return None;
    }
    let message = tokio::time::timeout_at(deadline, confirm::bounded(port.message(channel, id)))
        .await
        .ok()?
        .ok()??;
    if message.id != id || message.author_id != port.bot_id() || message.content != payload {
        return None;
    }
    u64::try_from(MessageId::new(id).created_at().timestamp_millis()).ok()
}

pub(super) async fn episode<P: DiscordPort>(
    root: &Path,
    pool: Option<&sqlx::PgPool>,
    port: &P,
    plan: &Manifest,
    deadline: Instant,
) -> Option<EpisodeCovered> {
    if !plan.completed || plan.input.is_none() {
        return None;
    }
    let local = tokio::time::timeout_at(
        deadline,
        local_snapshot(
            root.to_path_buf(),
            plan.pieces.first()?.key.channel_id,
            None,
        ),
    )
    .await
    .ok()
    .flatten()?;
    verify(pool, port, plan, 0, deadline, local.1.as_deref())
        .await
        .map(|posted_ms| EpisodeCovered { posted_ms })
}

type LocalSnapshot = (
    Option<std::time::SystemTime>,
    Option<Arc<crate::services::tui_o::store::ledger::LedgerState>>,
);

async fn local_snapshot(
    root: std::path::PathBuf,
    channel: u64,
    previous: Option<LocalSnapshot>,
) -> Option<LocalSnapshot> {
    tokio::task::spawn_blocking(move || {
        let stamp = delivery_evidence::stamp(&root, channel);
        if let Some(previous) = previous
            && previous.0 == stamp
        {
            return previous;
        }
        (
            stamp,
            delivery_evidence::local(&root, channel).map(Arc::new),
        )
    })
    .await
    .ok()
}

pub(in crate::services::discord) struct Frame<'a> {
    pub provider: &'a ProviderKind,
    pub channel: ChannelId,
    pub path: &'a Path,
    pub file: (u64, u64),
    pub range: (u64, u64),
    pub body: &'a str,
    pub sent: usize,
}

pub(in crate::services::discord) async fn tail(
    http: &Arc<serenity::Http>,
    shared: &SharedData,
    frame: Frame<'_>,
    deadline: Instant,
) -> Option<TailCovered> {
    let root = crate::config::runtime_root()?;
    let (build_root, provider, path) = (
        root.clone(),
        frame.provider.clone(),
        frame.path.to_path_buf(),
    );
    let channel = frame.channel.get();
    let (file, range) = (frame.file, frame.range);
    let plan = tokio::time::timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || {
            manifest::build(
                &build_root,
                manifest::Query {
                    provider: &provider,
                    channel,
                    path: &path,
                    file,
                    range,
                },
                deadline,
            )
        }),
    )
    .await
    .ok()?
    .ok()
    .flatten()?;
    if plan.body != frame.body {
        return None;
    }
    let bot = tokio::time::timeout_at(deadline, http.get_current_user())
        .await
        .ok()?
        .ok()?
        .id
        .get();
    let port = GatewayPort::new(Arc::clone(http), bot);
    let mut local = None;
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        local = tokio::time::timeout_at(deadline, local_snapshot(root.clone(), channel, local))
            .await
            .ok()
            .flatten();
        if tokio::time::timeout_at(
            deadline,
            verify(
                shared.pg_pool.as_ref(),
                &port,
                &plan,
                frame.sent,
                deadline,
                local.as_ref().and_then(|snapshot| snapshot.1.as_deref()),
            ),
        )
        .await
        .ok()
        .flatten()
        .is_some()
        {
            return Some(TailCovered { _private: () });
        }
        let pause = deadline
            .saturating_duration_since(Instant::now())
            .min(std::time::Duration::from_secs(1));
        tokio::time::sleep(pause).await;
    }
}

#[cfg(test)]
#[path = "o_delivery_evidence_tests.rs"]
pub(in crate::services::discord) mod tests;
