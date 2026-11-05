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
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::time::Instant;

pub(in crate::services::discord) use super::o_delivery_manifest::BodyProducer;

#[derive(Debug)]
pub(in crate::services::discord) struct TailCovered {
    _private: (),
}

pub(super) struct EpisodeCovered {
    pub posted_ms: u64,
}

struct SharedRead {
    task: Option<tokio::task::JoinHandle<()>>,
    result: tokio::sync::oneshot::Receiver<Option<Vec<u64>>>,
}

impl Drop for SharedRead {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[derive(Default)]
struct SharedReads {
    pending: Mutex<HashMap<usize, SharedRead>>,
    ready: Arc<tokio::sync::Notify>,
    poll: bool,
}

impl SharedReads {
    fn candidates(
        &self,
        pool: &sqlx::PgPool,
        position: usize,
        key: PieceKey,
        sender: u64,
        payload: String,
        deadline: Instant,
    ) -> Option<Vec<u64>> {
        let mut reads = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(read) = reads.get_mut(&position) {
            match read.result.try_recv() {
                Ok(result) => {
                    if let Some(task) = read.task.take() {
                        task.abort();
                    }
                    return result;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    if let Some(task) = read.task.take() {
                        task.abort();
                    }
                    if !self.poll {
                        return None;
                    }
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return None,
            }
        }
        // New pieces can advance immediately; only another read of a finished piece waits for a poll.
        if reads.values().filter(|read| read.task.is_some()).count() < 8 {
            let (done, result) = tokio::sync::oneshot::channel();
            let (pool, ready) = (pool.clone(), Arc::clone(&self.ready));
            let task = tokio::spawn(async move {
                let result = tokio::time::timeout_at(
                    deadline,
                    delivery_candidates(&pool, &key, sender, &payload),
                )
                .await
                .ok()
                .and_then(Result::ok);
                let found = result.as_ref().is_some_and(|ids| !ids.is_empty());
                let _ = done.send(result);
                if found {
                    ready.notify_one();
                }
            });
            reads.insert(
                position,
                SharedRead {
                    task: Some(task),
                    result,
                },
            );
        }
        None
    }

    fn forget(&self, position: usize) {
        drop(
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&position),
        );
    }
}

// These results belong only to this immutable manifest lookup, including its final source check.
#[derive(Default)]
struct Verification {
    exact: HashMap<usize, u64>,
    shared: Option<SharedReads>,
}

async fn verify<P: DiscordPort>(
    pool: Option<&sqlx::PgPool>,
    port: &P,
    plan: &Manifest,
    sent: usize,
    deadline: Instant,
    local: Option<&delivery_evidence::LocalEvidence>,
    progress: &mut Verification,
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
        .enumerate()
        .filter(|(_, piece)| piece.body_end > sent)
        .map(|(position, piece)| (position, piece.clone()))
        .collect();
    if required.is_empty() {
        return None;
    }
    let pending: Vec<_> = required
        .iter()
        .filter(|(position, _)| !progress.exact.contains_key(position))
        .cloned()
        .collect();
    let shared_reads = progress.shared.as_ref();
    let checks = pending.into_iter().map(|(position, piece)| async move {
        let proof = async {
            let mut ids = Vec::new();
            if let Some(local) = &local
                && let Some((_, record)) = local.latest_piece(&piece.key, piece.index)
                && record.payload == piece.payload
                && let Some(PieceOutcome::Posted(id)) = record.outcome
            {
                if local.confirmed_post(&piece.key, piece.index, port.bot_id()) == Some(id) {
                    return u64::try_from(MessageId::new(id).created_at().timestamp_millis()).ok();
                }
                ids.push(id);
            }
            for &id in &ids {
                if let Some(time) = exact(port, channel, id, &piece.payload, deadline).await {
                    return Some(time);
                }
            }
            ids.clear();
            if let (Some(pool), Some(key)) = (pool, PieceKey::new(piece.key.clone(), piece.index)) {
                // A slow shared read must not hold the next local receipt snapshot behind it.
                let shared = match shared_reads {
                    Some(reads) => reads.candidates(
                        pool,
                        position,
                        key,
                        port.bot_id(),
                        piece.payload.clone(),
                        deadline,
                    ),
                    None => tokio::time::timeout_at(
                        deadline,
                        delivery_candidates(pool, &key, port.bot_id(), &piece.payload),
                    )
                    .await
                    .ok()
                    .and_then(Result::ok),
                };
                ids.extend(shared.unwrap_or_default());
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
        }
        .await;
        (position, proof)
    });
    let checks = futures::stream::iter(checks).buffer_unordered(8);
    futures::pin_mut!(checks);
    while let Some((position, proof)) = checks.next().await {
        if let Some(posted_ms) = proof {
            progress.exact.insert(position, posted_ms);
            if let Some(reads) = &progress.shared {
                reads.forget(position);
            }
        }
    }
    required
        .iter()
        .map(|(position, _)| progress.exact.get(position).copied())
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
    verify(
        pool,
        port,
        plan,
        0,
        deadline,
        local.1.as_deref(),
        &mut Verification::default(),
    )
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
    let mut progress = Verification {
        shared: Some(SharedReads {
            poll: true,
            ..SharedReads::default()
        }),
        ..Verification::default()
    };
    let mut next_poll = Instant::now();
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
                &mut progress,
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
        if let Some(reads) = &mut progress.shared {
            if reads.poll {
                next_poll = deadline.min(Instant::now() + std::time::Duration::from_secs(1));
            }
            tokio::select! {
                _ = tokio::time::sleep_until(next_poll) => reads.poll = true,
                _ = reads.ready.notified() => reads.poll = false,
            }
        }
    }
}

/// Claude can date a late body record after the bridge queued that body on the same host.
const HEADLESS_OUTBOX_SKEW_MS: i64 = 5_000;

/// The bridge queues the Discord-formatted body, once prefixed by a single session-banner line.
fn headless_content_matches(content: &str, bodies: &[&str]) -> bool {
    bodies.iter().any(|body| {
        !body.trim().is_empty()
            && (content == *body
                || content
                    .split_once("\n\n")
                    .is_some_and(|(line, rest)| !line.contains('\n') && rest == *body))
    })
}

/// A headless bridge body leaves a sent `message_outbox` row, not an O receipt; only a row the
/// frame's own turn queued with this body after its records covers it, never another turn's.
/// `turns` are the held turn nonces in preference order, each with where another stored turn
/// starts: a turn speaks for the body only if every body record ends by that start.
pub(in crate::services::discord) async fn headless_outbox(
    shared: &SharedData,
    frame: Frame<'_>,
    producer: manifest::BodyProducer,
    turns: &[(&str, Option<u64>)],
    deadline: Instant,
) -> Option<TailCovered> {
    if turns.iter().all(|(nonce, _)| nonce.is_empty()) {
        return None;
    }
    let pool = shared.pg_pool.as_ref()?;
    let (provider, path) = (frame.provider.clone(), frame.path.to_path_buf());
    let (channel, file, range) = (frame.channel.get(), frame.file, frame.range);
    let body = frame.body.to_string();
    let written = tokio::time::timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || {
            manifest::body_written(
                manifest::Query {
                    provider: &provider,
                    channel,
                    path: &path,
                    file,
                    range,
                },
                &body,
                producer,
                deadline,
            )
        }),
    )
    .await
    .ok()?
    .ok()??;
    let producer_turn_nonce = turns
        .iter()
        .find(|(nonce, next_turn_start)| {
            !nonce.is_empty() && next_turn_start.is_none_or(|start| written.records_end <= start)
        })
        .map(|(nonce, _)| *nonce)?;
    let floor = chrono::DateTime::from_timestamp_millis(written.at_ms - HEADLESS_OUTBOX_SKEW_MS)?;
    let rows: Vec<String> = tokio::time::timeout_at(
        deadline,
        sqlx::query_scalar(
            "SELECT content FROM message_outbox
              WHERE target = $1 AND source = 'headless_turn' AND status = 'sent'
                AND created_at >= $2 AND producer_turn_nonce = $3
              ORDER BY id DESC LIMIT 32",
        )
        .bind(format!("channel:{channel}"))
        .bind(floor)
        .bind(producer_turn_nonce)
        .fetch_all(pool),
    )
    .await
    .ok()?
    .ok()?;
    let formatted = crate::services::discord::formatting::format_for_discord_with_status_panel(
        frame.body,
        frame.provider,
    );
    let bodies = [frame.body, formatted.as_str()];
    if !rows
        .iter()
        .any(|content| headless_content_matches(content, &bodies))
    {
        return None;
    }
    tracing::info!(
        channel_id = channel,
        frame_start = range.0,
        frame_end = range.1,
        body_bytes = frame.body.len(),
        "terminal frame covered by its headless outbox delivery"
    );
    Some(TailCovered { _private: () })
}

#[cfg(test)]
#[path = "o_delivery_evidence_tests.rs"]
pub(in crate::services::discord) mod tests;
