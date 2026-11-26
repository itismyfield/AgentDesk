use std::collections::HashMap;
use std::fs::File;

use sqlx::PgPool;

use super::{HealthRegistry, RelaySinkError, StreamFrame};
use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::provider::{ProviderKind, WrapperTerminalRead, wrapper_terminal_reads};
use crate::services::tmux_common::spawn_markers::read_generation_file_mtime_ns;
use crate::services::tmux_common::try_with_tmux_source_authority;

pub(super) struct Source<'a> {
    pub(super) session: &'a str,
    pub(super) path: &'a str,
    pub(super) generation: i64,
    pub(super) file: SourceFileIdentity,
}

impl Source<'_> {
    pub(super) fn current_through(&self, end: u64) -> bool {
        try_with_tmux_source_authority(self.session, |_| {
            self.generation == read_generation_file_mtime_ns(self.session)
                && File::open(self.path).is_ok_and(|file| {
                    SourceFileIdentity::from_open_file(&file) == self.file
                        && file.metadata().is_ok_and(|meta| meta.len() >= end)
                })
        })
        .unwrap_or(false)
    }
}

#[derive(Default)]
pub(super) struct HeadlessReads {
    recorded_session: bool,
    recorded_source: bool,
    ranges: Vec<(WrapperTerminalRead, bool)>,
}

pub(super) enum Ownership {
    Sent(u64),
    Pending,
    Unclaimed,
}

impl HeadlessReads {
    pub(super) fn recorded_source(&self) -> bool {
        self.recorded_source
    }

    pub(super) fn at_cursor(&self, cursor: u64) -> Ownership {
        self.ownership(|read| read.start <= cursor && cursor < read.end)
    }

    pub(super) fn next_sent_start(&self, cursor: u64) -> Option<u64> {
        let (read, sent) = self
            .ranges
            .iter()
            .filter(|(read, _)| read.start > cursor)
            .min_by_key(|(read, sent)| (read.start, *sent))?;
        sent.then_some(read.start)
    }

    pub(super) fn at_terminal_end(&self, end: u64) -> Ownership {
        self.ownership(|read| read.end == end)
    }

    fn ownership(&self, matches: impl Fn(&WrapperTerminalRead) -> bool) -> Ownership {
        let mut pending = false;
        for (read, sent) in &self.ranges {
            if matches(read) {
                if *sent {
                    return Ownership::Sent(read.end);
                }
                pending = true;
            }
        }
        if pending {
            Ownership::Pending
        } else {
            Ownership::Unclaimed
        }
    }
}

pub(super) async fn read_proofs(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    channel: u64,
    source: &Source<'_>,
    (start, end): (u64, u64),
) -> Result<HeadlessReads, RelaySinkError> {
    let Some(pool) = pool.filter(|_| *provider == ProviderKind::Claude) else {
        return Ok(HeadlessReads::default());
    };
    let Some(mut reads) = wrapper_terminal_reads(source.session, source.generation, source.file)
        .map_err(RelaySinkError::Transient)?
    else {
        return Ok(HeadlessReads::default());
    };
    let recorded_source = !reads.is_empty();
    reads.retain(|read| read.start < read.end && read.start < end && read.end > start);
    if reads.is_empty() {
        return Ok(HeadlessReads {
            recorded_session: true,
            recorded_source,
            ranges: Vec::new(),
        });
    }
    let nonces: Vec<_> = reads.iter().map(|read| read.nonce.as_str()).collect();
    // The source file names the producer turn. Only the worker's row can confirm a post.
    let owners = sqlx::query_as::<_, (String, bool)>(
        "SELECT producer_turn_nonce, bool_or(status = 'sent') FROM message_outbox
         WHERE target = $1 AND source = 'headless_turn' AND reason_code = 'headless.delivery'
           AND producer_turn_nonce = ANY($2) AND status IN ('pending', 'processing', 'sent')
         GROUP BY producer_turn_nonce",
    )
    .bind(format!("channel:{channel}"))
    .bind(&nonces)
    .fetch_all(pool)
    .await
    .map_err(|error| RelaySinkError::Transient(error.to_string()))?;
    let owners: HashMap<_, _> = owners.into_iter().collect();
    let ranges = reads
        .into_iter()
        .filter_map(|read| {
            let sent = owners.get(&read.nonce).copied()?;
            Some((read, sent))
        })
        .collect();
    Ok(HeadlessReads {
        recorded_session: true,
        recorded_source,
        ranges,
    })
}

pub(super) enum IdleFrame<'a> {
    Legacy,
    Ordered(Source<'a>, HeadlessReads),
    Stale,
}

pub(super) async fn idle_frame<'a>(
    registry: &HealthRegistry,
    frame: &'a StreamFrame,
) -> Result<IdleFrame<'a>, RelaySinkError> {
    let (Some((start, end)), Some(generation), Some(stamp)) = (
        frame.relay_range,
        frame.relay_generation_mtime_ns,
        frame.relay_source_stamp,
    ) else {
        return Ok(IdleFrame::Legacy);
    };
    let provider = &frame.binding.provider;
    if *provider != ProviderKind::Claude {
        return Ok(IdleFrame::Legacy);
    }
    let Ok(channel) = frame.binding.channel_id.parse() else {
        return Ok(IdleFrame::Legacy);
    };
    let Some(shared) = registry.shared_for_provider(provider).await else {
        return Ok(IdleFrame::Legacy);
    };
    let source = Source {
        session: &frame.session_name,
        path: &frame.binding.expected_rollout_path,
        generation,
        file: stamp.file,
    };
    let proof = read_proofs(
        shared.pg_pool.as_ref(),
        provider,
        channel,
        &source,
        (start, end),
    )
    .await?;
    if proof.recorded_session && !source.current_through(end) {
        return Ok(IdleFrame::Stale);
    }
    if !proof.recorded_source() || end.checked_sub(start) != Some(frame.payload.len() as u64) {
        return Ok(IdleFrame::Legacy);
    }
    Ok(IdleFrame::Ordered(source, proof))
}
