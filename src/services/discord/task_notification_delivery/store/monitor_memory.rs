//! Debug/test counterpart of the PostgreSQL observation receipt contract.
use super::*;

#[derive(Clone)]
struct Receipt {
    scope: TaskCardScope,
    observation: Observation,
    message: u64,
    bot: String,
}
static RECEIPTS: LazyLock<Mutex<Vec<Receipt>>> = LazyLock::new(|| Mutex::new(Vec::new()));

pub(in crate::services::discord::task_notification_delivery::store) fn record(
    claimed: &ClaimedCard,
    message: u64,
) -> Result<(), String> {
    let Some(observation) = &claimed.monitor else {
        return Ok(());
    };
    let mut receipts = RECEIPTS.lock().map_err(|e| e.to_string())?;
    if !receipts.iter().any(|r| {
        r.scope == claimed.scope
            && r.observation.source == observation.source
            && r.observation.payload == observation.payload
    }) {
        receipts.push(Receipt {
            scope: claimed.scope.clone(),
            observation: observation.clone(),
            message,
            bot: claimed.bot_key.clone(),
        });
    }
    Ok(())
}

pub(in crate::services::discord::task_notification_delivery::store) fn claim(
    event: &TaskCardEvent,
    bot: &str,
) -> Result<CardClaim, String> {
    let payload = event.payload.monitor_fingerprint();
    let source = event.scope.terminal_delivery_fingerprint.clone();
    let mut receipts = RECEIPTS.lock().map_err(|e| e.to_string())?;
    let found = receipts.iter_mut().find(|r| {
        r.scope.channel_id == event.scope.channel_id
            && r.scope.provider == event.scope.provider
            && ((source.is_some() && r.observation.source == source)
                || (r.scope.event_key == event.scope.event_key
                    && r.observation.payload == payload
                    && (source.is_none() || r.observation.source.is_none())))
    });
    if let Some(r) = found {
        if r.observation.source.is_none() {
            r.observation.source = source;
        }
        return Ok(CardClaim::Existing {
            message_id: r.message,
            bot_key: r.bot.clone(),
        });
    }
    drop(receipts);
    let mut rows = MEMORY_STORE.lock().map_err(|e| e.to_string())?;
    let mut scope = event.scope.clone();
    scope.terminal_delivery_fingerprint = Some(source.clone().unwrap_or_else(|| payload.clone()));
    let row = rows.entry(scope.clone()).or_insert_with(|| MemoryRow {
        state: MemoryState::Posting,
        bot_key: bot.to_string(),
        nonce: stable_nonce(&scope, 1),
        message_id: None,
        revision: 1,
        update_count: 1,
        rendered_content: event.payload.render(1),
        content_hash: super::content_hash(&event.payload.render(1)),
        terminal_delivery_fingerprint: scope.terminal_delivery_fingerprint.clone(),
        lease_owner: None,
        lease_expires_at: None,
        post_started_at: None,
        last_error: None,
    });
    if row.lease_owner.is_some() && row.lease_expires_at.is_some_and(|t| t > Instant::now()) {
        return Ok(CardClaim::Busy {
            bot_key: row.bot_key.clone(),
        });
    }
    if row.state == MemoryState::CardPosted
        && row.terminal_delivery_fingerprint == scope.terminal_delivery_fingerprint
    {
        return Ok(CardClaim::Existing {
            message_id: row.message_id.ok_or("missing memory receipt")?,
            bot_key: row.bot_key.clone(),
        });
    }
    let editing = row.state == MemoryState::CardPosted && row.bot_key == bot;
    let mut count = row.update_count;
    if row.state == MemoryState::CardPosted {
        count = count.saturating_add(1);
    }
    if row.state == MemoryState::CardPosted && !editing {
        row.revision = row.revision.saturating_add(1);
        row.state = MemoryState::Posting;
        row.message_id = None;
        row.nonce = stable_nonce(&scope, row.revision);
        row.post_started_at = None;
    }
    if row.post_started_at.is_none() && !editing {
        row.bot_key = bot.to_string();
        row.rendered_content = event.payload.render(count);
        row.update_count = count;
        row.content_hash = super::content_hash(&row.rendered_content);
        row.terminal_delivery_fingerprint = scope.terminal_delivery_fingerprint.clone();
    }
    let owner = uuid::Uuid::new_v4().to_string();
    row.lease_owner = Some(owner.clone());
    row.lease_expires_at = Some(Instant::now() + Duration::from_secs(LEASE_SECONDS as u64));
    let action = if editing {
        ClaimAction::Edit {
            message_id: row.message_id.ok_or("missing memory edit message")?,
        }
    } else {
        ClaimAction::Post
    };
    let mut claimed = memory_claim(&scope, &owner, row, action);
    if row.post_started_at.is_none()
        || row.terminal_delivery_fingerprint == scope.terminal_delivery_fingerprint
    {
        claimed.scope.terminal_delivery_fingerprint = scope.terminal_delivery_fingerprint;
        claimed.monitor = Some(Observation { payload, source });
    }
    claimed.update_count = count;
    Ok(CardClaim::Owned(claimed))
}
