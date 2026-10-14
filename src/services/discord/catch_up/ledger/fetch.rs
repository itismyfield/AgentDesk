//! Walk every page backward from one finite snapshot before granting coverage.

use poise::serenity_prelude::{ChannelId, Message};

use super::super::{CATCH_UP_FETCH_LIMIT, CatchUpDiscordApi, CatchUpFetchRequest};
use super::{Coverage, Incomplete, LedgerFetchResult};

pub(in crate::services::discord::catch_up) async fn fetch<A: CatchUpDiscordApi + ?Sized>(
    api: &A,
    channel_id: ChannelId,
    lower_exclusive: Option<u64>,
    max_pages: usize,
) -> LedgerFetchResult {
    let mut result = LedgerFetchResult {
        messages: Vec::new(),
        lower_exclusive,
        horizon: None,
        coverage: Coverage::Incomplete(Incomplete::BudgetExhausted),
    };
    if max_pages == 0 {
        return result;
    }
    let snapshot = match api
        .fetch_messages(channel_id, CatchUpFetchRequest::new(1))
        .await
    {
        Ok(page) => page.into_iter().max_by_key(|message| message.id.get()),
        Err(error) => {
            result.coverage = Coverage::Incomplete(Incomplete::FetchFailed(error));
            return result;
        }
    };
    let mut cursor = if let Some(latest) = snapshot {
        let horizon = latest.id.get();
        result.horizon = Some(horizon);
        if lower_exclusive.is_some_and(|lower| horizon <= lower) {
            result.coverage = Coverage::Complete;
            return result;
        }
        result.messages.push(latest);
        horizon
    } else {
        result.coverage = exhausted(lower_exclusive);
        return result;
    };
    for _ in 1..max_pages {
        let page = match api
            .fetch_messages(
                channel_id,
                CatchUpFetchRequest::new(CATCH_UP_FETCH_LIMIT).before(cursor),
            )
            .await
        {
            Ok(page) => page,
            Err(error) => {
                result.coverage = Coverage::Incomplete(Incomplete::FetchFailed(error));
                break;
            }
        };
        if page.is_empty() {
            result.coverage = exhausted(lower_exclusive);
            break;
        }
        let mut older: Vec<Message> = page
            .into_iter()
            .filter(|message| message.id.get() < cursor)
            .collect();
        let Some(next) = older.iter().map(|message| message.id.get()).min() else {
            result.coverage = Coverage::Incomplete(Incomplete::CursorNotAdvancing);
            break;
        };
        let reached_lower = lower_exclusive.is_some_and(|lower| next <= lower);
        older.retain(|message| lower_exclusive.is_none_or(|lower| message.id.get() > lower));
        result.messages.extend(older);
        if reached_lower {
            result.coverage = Coverage::Complete;
            break;
        }
        // A short nonempty page alone does not prove that the lower bound was reached.
        cursor = next;
    }
    result.messages.sort_by_key(|message| message.id.get());
    result.messages.dedup_by_key(|message| message.id.get());
    result
}

fn exhausted(lower_exclusive: Option<u64>) -> Coverage {
    if lower_exclusive.is_some() {
        Coverage::Complete
    } else {
        Coverage::Incomplete(Incomplete::UnknownLowerBound)
    }
}
