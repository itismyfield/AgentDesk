//! The one key every consumer gives a rejected body: provider, channel and the digest of the
//! admitted input's turn nonce, unchanged by the consumer, the owner or the generation.
use crate::db::tui_output_disposition::Identity;

/// Marks the identity as the canonical episode form, apart from a consumer-local view's key.
const EPISODE_SOURCE: &str = "episode:v1";

/// The canonical identity of `turn_nonce`'s body, or none for a view with no turn nonce.
pub(crate) fn identity(provider: &str, channel: u64, turn_nonce: &str) -> Option<Identity> {
    let nonce = turn_nonce.trim();
    if provider.trim().is_empty() || channel == 0 || nonce.is_empty() {
        return None;
    }
    Some(Identity {
        channel,
        input: 0,
        nonce: super::first_prompt_digest(nonce),
        execution: provider.trim().to_ascii_lowercase(),
        source: EPISODE_SOURCE.into(),
    })
}
