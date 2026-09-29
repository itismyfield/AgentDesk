//! POST-then-read round trip proving Discord stores each case exactly as sent, as settlement
//! assumes. It bypasses the ledger, so it only targets an approved test channel.

use super::{DiscordPort, PostOutcome};
use crate::services::discord::formatting::split_for_shadow;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundTrip {
    pub case: String,
    pub sent: String,
    pub posted: Option<u64>,
    /// What history returned for the posted id; `None` when the POST or the read failed.
    pub echoed: Option<String>,
    pub error: Option<String>,
}

impl RoundTrip {
    pub fn matched(&self) -> bool {
        self.echoed.as_deref() == Some(self.sent.as_str())
    }
}

/// Plain, code fence, emoji and a mention, then texts split at the 2000-unit limit: plain, UTF-16
/// surrogate pairs and a reopened code fence. Each case is what the writer would post.
pub fn cases(bot_id: u64) -> Vec<(String, String)> {
    let samples = [
        ("plain", "O writer round trip: plain text.".to_string()),
        (
            "code_fence",
            "```rust\nfn main() {\n    println!(\"hi\");\n}\n```".into(),
        ),
        ("emoji", "O writer round trip 🚀 ✅ 👩‍💻 🇰🇷".into()),
        ("mention", format!("O writer round trip <@{bot_id}>")),
        ("limit", "a".repeat(4000)),
        ("limit_utf16", "👍".repeat(1500)),
        (
            "limit_fence",
            format!("```text\n{}\n```", "code line\n".repeat(400)),
        ),
    ];
    let mut out = Vec::new();
    for (name, text) in samples {
        for (index, (piece, _)) in split_for_shadow(text.trim()).into_iter().enumerate() {
            out.push((format!("{name}#{index}"), piece));
        }
    }
    out
}

/// Posts every case once and reads it back from history by id.
pub async fn round_trip<P: DiscordPort>(port: &P, channel: u64) -> Vec<RoundTrip> {
    let mut trips = Vec::new();
    for (case, sent) in cases(port.bot_id()) {
        let mut trip = RoundTrip {
            case,
            sent: sent.clone(),
            posted: None,
            echoed: None,
            error: None,
        };
        match port.post(channel, sent).await {
            PostOutcome::Created(message) => trip.posted = Some(message.id),
            other => trip.error = Some(format!("{other:?}")),
        }
        if let Some(id) = trip.posted {
            match port.history_after(channel, id.saturating_sub(1)).await {
                Ok(page) => {
                    let found = page.into_iter().find(|message| message.id == id);
                    trip.echoed = found.map(|message| message.content);
                }
                Err(error) => trip.error = Some(error),
            }
        }
        trips.push(trip);
    }
    trips
}
