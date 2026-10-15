//! Settling a POST whose result is unknown: read history after the anchor and match the exact
//! payload from this bot. Nothing here posts, and "not seen" never licenses a repost.

use std::future::Future;
use std::time::Duration;

use super::{DiscordPort, SeenMessage};

/// First look after the request ended or the process restarted.
pub const FIRST_LOOK: Duration = Duration::from_secs(10);
/// Second look; no candidate by then, with history proven readable, is `NotFound`.
pub const LAST_LOOK: Duration = Duration::from_secs(30);
pub const HISTORY_PAGE: usize = 100;
pub const MAX_PAGES: usize = 10;
/// Bounds each read, rate-limit wait and body decode included.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounds a whole settlement, so it ends inside the delivery lease held over the POST.
pub(crate) const SETTLE_DEADLINE: Duration = Duration::from_secs(110);
/// How many of this bot's latest posts an empty history may be checked against.
pub(crate) const PROOF_IDS: usize = 3;

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

/// A read that fails or outlasts `READ_TIMEOUT` is an error, never an empty answer.
pub(crate) async fn bounded<T>(read: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    let late = |_| Err(format!("read exceeded {READ_TIMEOUT:?}"));
    tokio::time::timeout(READ_TIMEOUT, read)
        .await
        .unwrap_or_else(late)
}

/// Reads known messages by id in order until `proves` takes one: a 404 or a message that proves
/// nothing tries the next, a failed or late read stops. `Ok(None)` means none proved anything.
pub(crate) async fn first_proof<M, T, R>(
    ids: &[u64],
    read: impl Fn(u64) -> R,
    proves: impl Fn(u64, M) -> Option<T>,
) -> Result<Option<T>, String>
where
    R: Future<Output = Result<Option<M>, String>>,
{
    for &id in ids {
        if let Some(proof) = bounded(read(id)).await?.and_then(|read| proves(id, read)) {
            return Ok(Some(proof));
        }
    }
    Ok(None)
}

/// What one bounded scan after the anchor saw.
#[derive(Default)]
struct Seen {
    /// This bot's messages whose content equals the payload, oldest first.
    exact: Vec<u64>,
    /// This bot's messages with other content: Discord may have changed the payload's text.
    altered: bool,
    /// Any message at all was listed, which only a readable history returns.
    listed: bool,
}

async fn scan<P: DiscordPort>(
    port: &P,
    channel: u64,
    anchor: u64,
    payload: &str,
) -> Result<Seen, String> {
    let (mut after, mut seen) = (anchor, Seen::default());
    for _ in 0..MAX_PAGES {
        let mut page = bounded(port.history_after(channel, after))
            .await
            .map_err(|error| format!("history read failed: {error}"))?;
        page.sort_by_key(|message| message.id);
        seen.listed |= !page.is_empty();
        for message in page.iter().filter(|m| m.author_id == port.bot_id()) {
            if message.content == payload {
                seen.exact.push(message.id);
            } else {
                seen.altered = true;
            }
        }
        match page.last() {
            Some(last) if page.len() >= HISTORY_PAGE => after = last.id,
            _ => return Ok(seen),
        }
    }
    Err(format!(
        "more than {MAX_PAGES} history pages after the anchor"
    ))
}

/// The last look found no exact candidate. An empty listing is also what Discord returns without
/// history permission, so it counts as absence only after one of `proof_ids` reads back by id.
async fn absent<P: DiscordPort>(port: &P, channel: u64, seen: &Seen, proof_ids: &[u64]) -> Verdict {
    if seen.altered {
        return Verdict::Unresolved("same-author non-exact message observed".into());
    }
    if seen.listed {
        return Verdict::NotFound;
    }
    let bot = port.bot_id();
    let own = |id: u64, read: SeenMessage| (read.id == id && read.author_id == bot).then_some(());
    match first_proof(proof_ids, |id| port.message(channel, id), own).await {
        Ok(Some(())) => Verdict::NotFound,
        Ok(None) => Verdict::Unresolved("empty history without read permission proof".into()),
        Err(error) => Verdict::Unresolved(format!("permission proof read failed: {error}")),
    }
}

/// Looks at `FIRST_LOOK` and again at `LAST_LOOK`, all within `SETTLE_DEADLINE`. A failed first
/// read waits for the last look, whose own failure is `Unresolved`.
pub async fn settle<P: DiscordPort>(
    port: &P,
    channel: u64,
    anchor: u64,
    payload: &str,
    earlier_same_payload: bool,
    proof_ids: &[u64],
) -> Verdict {
    let looks = async {
        tokio::time::sleep(FIRST_LOOK).await;
        if let Ok(seen) = scan(port, channel, anchor, payload).await
            && let Some(verdict) = judge(&seen.exact, earlier_same_payload)
        {
            return verdict;
        }
        tokio::time::sleep(LAST_LOOK - FIRST_LOOK).await;
        match scan(port, channel, anchor, payload).await {
            Err(reason) => Verdict::Unresolved(reason),
            Ok(seen) => match judge(&seen.exact, earlier_same_payload) {
                Some(verdict) => verdict,
                None => absent(port, channel, &seen, proof_ids).await,
            },
        }
    };
    let late = |_| Verdict::Unresolved(format!("settlement exceeded {SETTLE_DEADLINE:?}"));
    tokio::time::timeout(SETTLE_DEADLINE, looks)
        .await
        .unwrap_or_else(late)
}
