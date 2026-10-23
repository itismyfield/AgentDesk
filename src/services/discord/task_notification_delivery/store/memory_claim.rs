//! In-memory debug counterpart of the durable task-card claim.
use super::*;

pub(super) fn claim_card_memory(
    scope: &TaskCardScope,
    preferred_bot_key: &str,
    seed_content: &str,
    seed_hash: &str,
    intent: StoreIntent,
) -> Result<CardClaim, String> {
    if let Some((message_id, bot_key)) = find_terminal_delivery_memory(scope, seed_hash)? {
        return Ok(CardClaim::Existing {
            message_id,
            bot_key,
        });
    }
    let mut rows = MEMORY_STORE
        .lock()
        .map_err(|_| "task card memory store poisoned".to_string())?;
    let lease_owner = uuid::Uuid::new_v4().to_string();
    let now = Instant::now();
    let lease_expires_at = now + Duration::from_secs(LEASE_SECONDS as u64);
    let row = rows.entry(scope.clone()).or_insert_with(|| MemoryRow {
        state: MemoryState::Posting,
        bot_key: preferred_bot_key.to_string(),
        nonce: stable_nonce(scope, 1),
        message_id: None,
        revision: 1,
        update_count: 1,
        rendered_content: seed_content.to_string(),
        content_hash: seed_hash.to_string(),
        terminal_delivery_fingerprint: scope.terminal_delivery_fingerprint.clone(),
        lease_owner: Some(lease_owner.clone()),
        lease_expires_at: Some(lease_expires_at),
        post_started_at: None,
        last_error: None,
    });
    if row.lease_owner.as_deref() == Some(lease_owner.as_str()) {
        return Ok(CardClaim::Owned(Box::new(memory_claim(
            scope,
            &lease_owner,
            row,
            ClaimAction::Post,
        ))));
    }
    let lease_active = row
        .lease_expires_at
        .is_some_and(|expires_at| expires_at > now)
        && row.lease_owner.is_some();
    if lease_active {
        return Ok(CardClaim::Busy {
            bot_key: row.bot_key.clone(),
        });
    }
    let exact_terminal_replay = scope.terminal_delivery_fingerprint.is_some()
        && row.terminal_delivery_fingerprint == scope.terminal_delivery_fingerprint;
    if row.state == MemoryState::CardPosted
        && (intent == StoreIntent::Promotion || exact_terminal_replay)
    {
        return Ok(CardClaim::Existing {
            message_id: row
                .message_id
                .ok_or_else(|| "memory card row omitted message id".to_string())?,
            bot_key: row.bot_key.clone(),
        });
    }
    row.lease_owner = Some(lease_owner.clone());
    row.lease_expires_at = Some(lease_expires_at);
    row.last_error = None;
    let new_terminal_completion = row.state == MemoryState::CardPosted
        && intent == StoreIntent::Observation
        && scope.terminal_delivery_fingerprint.is_some()
        && !exact_terminal_replay;
    let action = if new_terminal_completion {
        row.state = MemoryState::Posting;
        row.message_id = None;
        row.revision = row.revision.saturating_add(1);
        row.update_count = 1;
        row.rendered_content = seed_content.to_string();
        row.content_hash = seed_hash.to_string();
        row.terminal_delivery_fingerprint = scope.terminal_delivery_fingerprint.clone();
        row.nonce = stable_nonce(scope, row.revision);
        row.post_started_at = None;
        ClaimAction::Post
    } else if row.state == MemoryState::CardPosted {
        row.update_count = row.update_count.saturating_add(1);
        ClaimAction::Edit {
            message_id: row
                .message_id
                .ok_or_else(|| "memory card row omitted message id".to_string())?,
        }
    } else {
        let was_footer_only = row.state == MemoryState::FooterOnly;
        row.state = MemoryState::Posting;
        if was_footer_only {
            row.post_started_at = None;
        }
        if row.bot_key.is_empty() {
            row.bot_key = preferred_bot_key.to_string();
            row.nonce = stable_nonce(scope, row.revision);
        }
        if row.terminal_delivery_fingerprint.is_none() {
            row.terminal_delivery_fingerprint = scope.terminal_delivery_fingerprint.clone();
        }
        if intent == StoreIntent::Observation && row.post_started_at.is_none() {
            row.rendered_content = seed_content.to_string();
            row.content_hash = seed_hash.to_string();
        }
        ClaimAction::Post
    };
    let mut claimed = memory_claim(scope, &lease_owner, row, action);
    claimed.new_terminal_completion = new_terminal_completion;
    Ok(CardClaim::Owned(Box::new(claimed)))
}
