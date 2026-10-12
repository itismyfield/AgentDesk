//! Finish a held After range against a finite snapshot before earning release evidence.

use poise::serenity_prelude::{ChannelId, Message};

use super::{CATCH_UP_FETCH_LIMIT, CatchUpDiscordApi, CatchUpFetchRequest};

pub(super) async fn complete<A: CatchUpDiscordApi + ?Sized>(
    api: &A,
    channel_id: ChannelId,
    mut messages: Vec<Message>,
) -> Result<Vec<Message>, String> {
    if messages.len() < usize::from(CATCH_UP_FETCH_LIMIT) {
        return Ok(messages);
    }
    let mut cursor = messages
        .iter()
        .map(|message| message.id.get())
        .max()
        .unwrap_or_default();
    let upper = api
        .fetch_messages(channel_id, CatchUpFetchRequest::new(1))
        .await?
        .iter()
        .map(|message| message.id.get())
        .max()
        .ok_or_else(|| "held range latest-message snapshot was empty".to_owned())?
        .max(cursor);
    // New arrivals cannot extend this scan's upper bound indefinitely.
    while cursor < upper {
        let mut page = api
            .fetch_messages(
                channel_id,
                CatchUpFetchRequest::new(CATCH_UP_FETCH_LIMIT).after(cursor),
            )
            .await?;
        let full = page.len() >= usize::from(CATCH_UP_FETCH_LIMIT);
        page.retain(|message| message.id.get() <= upper);
        let Some(next) = page.iter().map(|message| message.id.get()).max() else {
            break;
        };
        if next <= cursor {
            return Err("held range After page made no cursor progress".to_owned());
        }
        cursor = next;
        messages.extend(page);
        if !full {
            break;
        }
    }
    Ok(messages)
}
