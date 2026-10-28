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
    local: Option<&delivery_evidence::LocalEvidence>,
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
            plan.pieces
                .iter()
                .map(|p| (p.key.clone(), p.index))
                .collect(),
            deadline,
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
    Option<Arc<delivery_evidence::LocalEvidence>>,
);

async fn local_snapshot(
    root: std::path::PathBuf,
    channel: u64,
    requested: Vec<(crate::services::tui_o::shadow::UnitKey, u32)>,
    deadline: Instant,
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
            delivery_evidence::local(&root, channel, &requested, deadline).map(Arc::new),
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

async fn terminal_manifest(root: &Path, frame: &Frame<'_>, deadline: Instant) -> Option<Manifest> {
    let (build_root, provider, path) = (
        root.to_path_buf(),
        frame.provider.clone(),
        frame.path.to_path_buf(),
    );
    let channel = frame.channel.get();
    let (file, range) = (frame.file, frame.range);
    tokio::time::timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || {
            manifest::build_terminal(
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
    .flatten()
}

fn unavailable(
    frame: &Frame<'_>,
    stage: &str,
    began: Instant,
    local_readable: Option<bool>,
) -> Option<TailCovered> {
    tracing::warn!(
        channel_id = frame.channel.get(),
        stage,
        local_readable = ?local_readable,
        elapsed_ms = began.elapsed().as_millis() as u64,
        frame_start = frame.range.0,
        frame_end = frame.range.1,
        body_bytes = frame.body.len(),
        "O terminal evidence unavailable"
    );
    None
}

pub(in crate::services::discord) async fn tail(
    http: &Arc<serenity::Http>,
    shared: &SharedData,
    frame: Frame<'_>,
    deadline: Instant,
) -> Option<TailCovered> {
    let began = Instant::now();
    let root = crate::config::runtime_root()?;
    let channel = frame.channel.get();
    let Some(plan) = terminal_manifest(&root, &frame, deadline).await else {
        return unavailable(&frame, "source_manifest", began, None);
    };
    if plan.range != frame.range {
        return unavailable(&frame, "manifest_range", began, None);
    }
    if plan.body != frame.body {
        tracing::warn!(
            channel_id = channel,
            manifest_bytes = plan.body.len(),
            frame_bytes = frame.body.len(),
            "O terminal manifest body differs"
        );
        return unavailable(&frame, "manifest_body", began, None);
    }
    let Ok(Ok(bot)) = tokio::time::timeout_at(deadline, http.get_current_user()).await else {
        return unavailable(&frame, "bot_identity", began, None);
    };
    let bot = bot.id.get();
    let port = GatewayPort::new(Arc::clone(http), bot);
    let mut local = None;
    loop {
        if Instant::now() >= deadline {
            return unavailable(
                &frame,
                "receipt_or_exact_get_deadline",
                began,
                Some(
                    local
                        .as_ref()
                        .is_some_and(|snapshot: &LocalSnapshot| snapshot.1.is_some()),
                ),
            );
        }
        local = tokio::time::timeout_at(
            deadline,
            local_snapshot(
                root.clone(),
                channel,
                plan.pieces
                    .iter()
                    .map(|p| (p.key.clone(), p.index))
                    .collect(),
                deadline,
                local,
            ),
        )
        .await
        .ok()
        .flatten();
        let covered = tokio::time::timeout_at(
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
        .is_some();
        if covered {
            let Some(current) = terminal_manifest(&root, &frame, deadline).await else {
                return unavailable(&frame, "source_revalidation", began, None);
            };
            return if current == plan && Instant::now() < deadline {
                Some(TailCovered { _private: () })
            } else {
                unavailable(&frame, "source_changed_or_expired", began, None)
            };
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
