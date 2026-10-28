//! A response create is retained independently of its mutable inflight mirror.
use super::inflight::{InflightEpisodePin, InflightTurnState};
use super::{SharedData, gateway::TurnGateway, inflight, runtime_store};
use poise::serenity_prelude::{ChannelId, MessageId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

mod store;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Part {
    nonce: String,
    text: String,
    reference: Option<(u64, u64)>,
    first_ms: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Attempt {
    version: u32,
    episode: String,
    source: super::outbound::delivery_record::ExactJsonlSourceIdentity,
    plan: String,
    scope: String,
    total: usize,
    accepted: Vec<(u64, usize)>,
    pending: Option<Part>,
    last: Option<Part>,
}

pub(super) enum CreateOutcome {
    Ready(PendingBind),
    Held(String),
}

type AfterBind = Box<dyn FnOnce(&SharedData, MessageId) -> bool + Send>;

pub(super) struct PendingBind {
    expected: Box<InflightTurnState>,
    path: PathBuf,
    plan: String,
    pub(super) anchor: MessageId,
    pub(super) len: usize,
    commit: Option<AfterBind>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn episode(row: &InflightTurnState) -> String {
    let mut hasher = Sha256::new();
    let nonce = row.turn_nonce.clone().unwrap_or_default();
    for field in [
        row.provider.clone(),
        row.channel_id.to_string(),
        row.request_owner_user_id.to_string(),
        row.user_msg_id.to_string(),
        row.turn_nonce.is_some().to_string(),
        nonce,
        row.born_generation.to_string(),
        row.started_at.clone(),
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}
fn matching_row(expected: &InflightTurnState) -> Result<inflight::LockedInflightEpisode, String> {
    let provider = expected.provider_kind().ok_or("provider missing")?;
    let fresh = inflight::load_inflight_state_read_only_result(&provider, expected.channel_id)?
        .ok_or("row missing")?;
    if !InflightEpisodePin::from_state(expected)
        .is_same_episode_as(&InflightEpisodePin::from_state(&fresh))
        || expected.tmux_session_name != fresh.tmux_session_name
        || expected.session_id != fresh.session_id
        || expected.delivery_record_owner_channel_id() != fresh.delivery_record_owner_channel_id()
        || expected.output_path != fresh.output_path
        || expected.turn_start_offset != fresh.turn_start_offset
        || expected.tui_terminal_generation_mtime_ns != fresh.tui_terminal_generation_mtime_ns
        || expected.tui_terminal_source_file_identity != fresh.tui_terminal_source_file_identity
        || expected.last_offset != fresh.last_offset
        || expected.response_sent_offset != fresh.response_sent_offset
        || expected.runtime_kind != fresh.runtime_kind
        || inflight::StreamRelayAuthority::from_state(expected)
            != inflight::StreamRelayAuthority::from_state(&fresh)
        || fresh.restart_mode.is_some()
        || fresh.rebind_origin
    {
        return Err("episode/source/owner changed".into());
    }
    inflight::lock_inflight_episode(
        &provider,
        fresh.channel_id,
        &InflightEpisodePin::from_state(&fresh),
    )
    .map_err(|_| "row advanced while locking".into())
}

impl PendingBind {
    pub(super) fn after_bind(
        mut self,
        commit: impl FnOnce(&SharedData, MessageId) -> bool + Send + 'static,
    ) -> Self {
        self.commit = Some(Box::new(commit));
        self
    }
    pub(super) fn bind_after_actor_check(
        self,
        shared: &SharedData,
        state: &mut InflightTurnState,
    ) -> inflight::GuardedSaveOutcome {
        use inflight::GuardedSaveOutcome::{AuthorityPinned, Saved};
        if !InflightEpisodePin::from_state(&self.expected).matches_state(state)
            || state.save_generation != self.expected.save_generation
        {
            return AuthorityPinned;
        }
        let Ok(guard) = matching_row(&self.expected) else {
            return AuthorityPinned;
        };
        let fresh = guard.state().clone();
        let Ok(Some(attempt)) = store::read(&self.path) else {
            return AuthorityPinned;
        };
        let expected_source = inflight::CodexRange::from_retained_tui_terminal(&self.expected)
            .map(|range| range.source);
        if attempt.version != 1
            || attempt.episode
                != format!(
                    "{}:{}",
                    episode(&self.expected),
                    self.expected.response_sent_offset
                )
            || expected_source.as_ref() != Some(&attempt.source)
            || attempt.scope != "response"
            || attempt.total != attempt.accepted.len()
            || attempt.pending.is_some()
            || attempt.accepted.iter().any(|p| p.0 == 0)
            || attempt.plan != self.plan
            || attempt.accepted.last().map(|p| p.0) != Some(self.anchor.get())
        {
            return AuthorityPinned;
        }
        if fresh.current_msg_id == self.anchor.get() {
            *state = fresh;
            return self.commit_bound(shared, state, guard);
        }
        if fresh.current_msg_id != self.expected.current_msg_id {
            return AuthorityPinned;
        }
        // The existing snapshot CAS repeats this exact pin after releasing the read lock.
        drop(guard);
        *state = fresh;
        let Some(provider) = state.provider_kind() else {
            return AuthorityPinned;
        };
        let outcome = inflight::bind_recovery_anchor_for_snapshot(
            &provider,
            state,
            self.anchor.get(),
            self.len,
        );
        if outcome != Saved {
            return outcome;
        }
        let Ok(guard) = matching_row(state) else {
            return AuthorityPinned;
        };
        self.commit_bound(shared, state, guard)
    }

    fn commit_bound(
        mut self,
        shared: &SharedData,
        state: &InflightTurnState,
        guard: inflight::LockedInflightEpisode,
    ) -> inflight::GuardedSaveOutcome {
        use inflight::GuardedSaveOutcome::{AuthorityPinned, Saved};
        let Some((root, provider)) = runtime_store::runtime_root().zip(state.provider_kind())
        else {
            return AuthorityPinned;
        };
        let Some(inflight_root) = inflight::inflight_runtime_root() else {
            return AuthorityPinned;
        };
        if guard.state().current_msg_id != self.anchor.get()
            || guard.state().save_generation != state.save_generation
            || store::confirm_existing(
                &root,
                &inflight::inflight_state_path(&inflight_root, &provider, state.channel_id),
            )
            .is_err()
        {
            return AuthorityPinned;
        }
        drop(guard);
        if self
            .commit
            .take()
            .is_none_or(|commit| commit(shared, self.anchor))
        {
            Saved
        } else {
            AuthorityPinned
        }
    }
}

/// This is only a transport ACK. The caller retains its pinned full-source lease.
pub(super) async fn create_terminal(
    shared: &std::sync::Arc<SharedData>,
    gateway: &dyn TurnGateway,
    row: &InflightTurnState,
    body: &str,
) -> CreateOutcome {
    let result = create_parts(shared, gateway, row, body).await;
    match result {
        Ok(bind) => CreateOutcome::Ready(bind),
        Err(reason) => {
            tracing::warn!(episode = %episode(row), scope = "response", attempt = %episode(row),
                reason = %reason, channel_id = row.channel_id, "body create held; delivery remains unconfirmed");
            CreateOutcome::Held(reason)
        }
    }
}

fn attempt_path(root: &std::path::Path, shared: &SharedData, row: &InflightTurnState) -> PathBuf {
    root.join("discord_body_creates")
        .join(&row.provider)
        .join(digest(shared.token_hash.as_bytes()))
        .join(row.channel_id.to_string())
        .join(format!(
            "{}-{}.json",
            episode(row),
            row.response_sent_offset
        ))
}
pub(super) fn has_attempt(shared: &SharedData, row: &InflightTurnState) -> bool {
    let Some(root) = runtime_store::runtime_root() else {
        return false;
    };
    !matches!(std::fs::symlink_metadata(attempt_path(&root, shared, row)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

pub(super) fn split_for_create(mut body: &str) -> Vec<String> {
    if !body.is_empty() && !super::formatting::needs_multiple_messages(body) {
        return vec![body.to_string()];
    }
    let mut chunks = Vec::new();
    while !body.is_empty() {
        // Keep the raw boundary bytes; the legacy display chunker strips newlines.
        let end = super::formatting::streaming_split_boundary(body, super::DISCORD_MSG_LIMIT - 10)
            .unwrap_or(body.len());
        chunks.push(body[..end].to_string());
        body = &body[end..];
    }
    chunks
}

async fn create_parts(
    shared: &std::sync::Arc<SharedData>,
    gateway: &dyn TurnGateway,
    row: &InflightTurnState,
    body: &str,
) -> Result<PendingBind, String> {
    let root = runtime_store::runtime_root().ok_or("runtime root missing")?;
    let source = inflight::CodexRange::from_retained_tui_terminal(row)
        .ok_or("pinned source missing")?
        .source;
    let key = format!("{}:{}", episode(row), row.response_sent_offset);
    let path = attempt_path(&root, shared, row);
    let chunks = split_for_create(body);
    if chunks.is_empty() || body.trim().is_empty() {
        return Err("empty body".into());
    }
    let plan = digest(&serde_json::to_vec(&chunks).map_err(|e| e.to_string())?);
    let mut verified = Vec::new();
    loop {
        let (attempt, part, ordinal) = {
            let row_guard = matching_row(row)?;
            let _attempt_guard = inflight::lock_inflight_state_path(&path)?;
            let mut attempt = match store::read(&path)? {
                Some(attempt) => attempt,
                None => Attempt {
                    version: 1,
                    episode: key.clone(),
                    source: source.clone(),
                    plan: plan.clone(),
                    scope: "response".into(),
                    total: chunks.len(),
                    accepted: Vec::new(),
                    pending: None,
                    last: None,
                },
            };
            if attempt.version != 1
                || attempt.episode != key
                || attempt.source != source
                || attempt.scope != "response"
            {
                return Err("unknown or foreign attempt".into());
            }
            let ids: std::collections::HashSet<_> = attempt.accepted.iter().map(|p| p.0).collect();
            if ids.contains(&0) || ids.len() != attempt.accepted.len() {
                return Err("invalid accepted ID list".into());
            }
            // A bound winner may have been legitimately EDITed after the initial ACK.
            if row_guard.state().current_msg_id != 0
                && attempt.accepted.len() == attempt.total
                && attempt.pending.is_none()
                && attempt
                    .accepted
                    .last()
                    .is_some_and(|p| p.0 == row_guard.state().current_msg_id)
            {
                return Ok(PendingBind {
                    expected: Box::new(row.clone()),
                    path: path.clone(),
                    plan: attempt.plan.clone(),
                    anchor: MessageId::new(row_guard.state().current_msg_id),
                    len: row_guard.state().current_msg_len,
                    commit: None,
                });
            }
            if row_guard.state().current_msg_id != row.current_msg_id
                || attempt.plan != plan
                || attempt.total != chunks.len()
            {
                return Err("unresolved response slot or split plan changed".into());
            }
            let ordinal = attempt.accepted.len();
            if ordinal > chunks.len() || attempt.accepted.iter().any(|p| p.0 == 0) {
                return Err("invalid accepted part list".into());
            }
            if ordinal == chunks.len() {
                store::publish(&root, &path, &attempt)?;
                drop(_attempt_guard);
                drop(row_guard);
                for (index, &(id, _)) in attempt.accepted.iter().enumerate() {
                    if verified.contains(&id) {
                        continue;
                    }
                    if id == 0 {
                        return Err("invalid accepted ID".into());
                    }
                    gateway
                        .verify_body_message(
                            shared,
                            ChannelId::new(row.channel_id),
                            MessageId::new(id),
                            &chunks[index],
                            None,
                        )
                        .await?;
                }
                let &(id, len) = attempt.accepted.last().ok_or("missing terminal part")?;
                matching_row(row)?;
                return Ok(PendingBind {
                    expected: Box::new(row.clone()),
                    path: path.clone(),
                    plan: plan.clone(),
                    anchor: MessageId::new(id),
                    len,
                    commit: None,
                });
            }
            if attempt.pending.is_none() {
                attempt.last = None;
                use base64::Engine;
                let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(&Sha256::digest(format!("{key}:response:{ordinal}").as_bytes())[..16]);
                attempt.pending = Some(Part {
                    nonce: format!("bc{nonce}"),
                    text: chunks[ordinal].clone(),
                    reference: None,
                    first_ms: chrono::Utc::now().timestamp_millis(),
                });
            }
            let part = attempt.pending.clone().ok_or("missing prepared part")?;
            if part.text != chunks[ordinal]
                || part.nonce.len() > 25
                || part.nonce.is_empty()
                || part.reference.is_some()
            {
                return Err("invalid frozen part".into());
            }
            // Reflush even a rename-visible record: an earlier process may have lost its flush ACK.
            store::publish(&root, &path, &attempt)?;
            (attempt, part, ordinal)
        };
        let now = chrono::Utc::now().timestamp_millis();
        if !within_replay_budget(part.first_ms, now) {
            tracing::warn!(episode = %key, scope = %attempt.scope, attempt = %part.nonce,
                first_ms = part.first_ms, "body create held: unknown ID outside recent nonce replay budget");
            return Err("unknown ID outside recent nonce replay budget".into());
        }
        let id = match gateway
            .create_body_message(
                shared,
                ChannelId::new(row.channel_id),
                &part.text,
                &part.nonce,
                part.reference,
            )
            .await
        {
            Ok(id) if id.get() != 0 => id,
            result => {
                tracing::warn!(episode = %key, scope = %attempt.scope, attempt = %part.nonce,
                    first_ms = part.first_ms, error = ?result, "body create result unknown; attempt retained");
                return Err("body create result unknown".into());
            }
        };
        let _attempt_guard = inflight::lock_inflight_state_path(&path)?;
        let mut fresh = store::read(&path)?.ok_or("attempt vanished during HTTP")?;
        if fresh.version != 1
            || fresh.episode != key
            || fresh.scope != "response"
            || fresh.plan != plan
            || fresh.source != source
            || fresh.total != chunks.len()
        {
            return Err("attempt changed during HTTP".into());
        }
        if fresh.accepted.len() == ordinal && fresh.pending.as_ref() == Some(&part) {
            if fresh.accepted.iter().any(|part| part.0 == id.get()) {
                return Err("one ID returned for distinct parts".into());
            }
            fresh.accepted.push((id.get(), part.text.len()));
            fresh.last = Some(part.clone());
            fresh.pending = None;
            store::publish(&root, &path, &fresh)?;
        } else if fresh.accepted.get(ordinal).map(|p| p.0) != Some(id.get()) {
            return Err("conflicting create result".into());
        }
        verified.push(id.get());
        drop(_attempt_guard);
        // Retain the returned effect even when the mutable row lost publication authority.
        matching_row(row)?;
    }
}

fn within_replay_budget(first_ms: i64, now_ms: i64) -> bool {
    now_ms
        .checked_sub(first_ms)
        .is_some_and(|age| (0..=60_000).contains(&age))
}
#[cfg(test)]
mod tests {
    #[test]
    fn raw_split_preserves_unicode_newlines_and_fences_with_bounded_parts() {
        let body = format!("\n```text\n{}\n```\n", "raw 🙂 line\n".repeat(900));
        let chunks = super::split_for_create(&body);
        assert!(chunks.len() > 2);
        assert_eq!(chunks.concat(), body);
        assert!(chunks.iter().all(|part| !part.is_empty()
            && super::super::formatting::discord_message_units(part) <= 1990));
    }
    #[test]
    fn replay_budget_accepts_exact_edge_refuses_rollback_and_overflow() {
        for (first, now, allowed) in [
            (1, 1, true),
            (1, 60_001, true),
            (1, 60_002, false),
            (2, 1, false),
            (i64::MIN, i64::MAX, false),
        ] {
            assert_eq!(
                super::within_replay_budget(first, now),
                allowed,
                "{first} -> {now}"
            );
        }
    }
}

/// Reconfirm a created terminal's exact receipt before its raw frontier can advance.
pub(super) fn confirm_created_receipt(
    source: &super::outbound::delivery_record::ExactJsonlSourceIdentity,
    message_id: u64,
) -> Result<(), String> {
    use super::outbound::delivery_record as dr;
    let root = runtime_store::runtime_root().ok_or("runtime root missing")?;
    let provider = super::super::provider::ProviderKind::from_str(&source.provider)
        .ok_or("provider missing")?;
    let path = dr::delivery_record_path(&provider, source.offset_authority_channel_id)
        .ok_or("receipt path missing")?;
    let _lock = dr::lock_record_path(&path)?;
    let record = dr::read_record_at(&path).ok_or("receipt record unreadable")?;
    if !source.is_authoritative()
        || message_id == 0
        || !record.confirmed_deliveries.iter().any(|receipt| {
            receipt.source == *source
                && receipt.message_id == message_id
                && receipt.delivery_channel_id == source.delivery_channel_id
        })
    {
        return Err("created receipt changed before flush confirmation".into());
    }
    store::confirm_existing(&root, &path)
}
