//! Settling a POST whose result is unknown: read history after the anchor and match the exact
//! payload from this bot. Nothing here posts, and "not seen" never licenses a repost.

use std::time::Duration;

use super::DiscordPort;

/// First look after the request ended or the process restarted.
pub const FIRST_LOOK: Duration = Duration::from_secs(10);
/// Second look; no candidate by then is `NotFound`.
pub const LAST_LOOK: Duration = Duration::from_secs(30);
pub const HISTORY_PAGE: usize = 100;
pub const MAX_PAGES: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Posted(u64),
    Ambiguous(Vec<u64>),
    NotFound,
    Unresolved(String),
}

/// One exact match settles; several, or one while an earlier unsettled piece had the same
/// payload, stay ambiguous. `None` means no candidate yet.
pub fn judge(candidates: &[u64], earlier_same_payload: bool) -> Option<Verdict> {
    match candidates {
        [] => None,
        [only] if !earlier_same_payload => Some(Verdict::Posted(*only)),
        many => Some(Verdict::Ambiguous(many.to_vec())),
    }
}

/// Ids of this bot's messages after `anchor` whose content equals `payload`, oldest first.
async fn scan<P: DiscordPort>(
    port: &P,
    channel: u64,
    anchor: u64,
    payload: &str,
) -> Result<Vec<u64>, String> {
    let (mut after, mut found) = (anchor, Vec::new());
    for _ in 0..MAX_PAGES {
        let mut page = port
            .history_after(channel, after)
            .await
            .map_err(|error| format!("history read failed: {error}"))?;
        page.sort_by_key(|message| message.id);
        let exact = page
            .iter()
            .filter(|m| m.author_id == port.bot_id() && m.content == payload);
        found.extend(exact.map(|message| message.id));
        match page.last() {
            Some(last) if page.len() >= HISTORY_PAGE => after = last.id,
            // Other authors' messages prove nothing about reading this bot's.
            _ if found.is_empty() && !port.history_readable(channel) => {
                return Err("no candidate without read permission proof".into());
            }
            _ => return Ok(found),
        }
    }
    Err(format!(
        "more than {MAX_PAGES} history pages after the anchor"
    ))
}

/// Looks at `FIRST_LOOK` and again at `LAST_LOOK`; a read that cannot be trusted is `Unresolved`.
pub async fn settle<P: DiscordPort>(
    port: &P,
    channel: u64,
    anchor: u64,
    payload: &str,
    earlier_same_payload: bool,
) -> Verdict {
    let mut waited = Duration::ZERO;
    for look in [FIRST_LOOK, LAST_LOOK] {
        tokio::time::sleep(look - waited).await;
        waited = look;
        match scan(port, channel, anchor, payload).await {
            Err(reason) => return Verdict::Unresolved(reason),
            Ok(candidates) => {
                if let Some(verdict) = judge(&candidates, earlier_same_payload) {
                    return verdict;
                }
            }
        }
    }
    Verdict::NotFound
}
