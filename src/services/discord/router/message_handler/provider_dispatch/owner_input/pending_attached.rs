//! A Claude Pending execution its driver attached after its launch's own logged start takes the
//! prompt the owner's Cold or HerdrIdle route waits for.
use crate::services::claude_tui::owner_input::Lineage;
use serde_json::Value;

/// Whether `lineage` is a Claude Pending its driver attached, naming the execution `target` names
/// for the turn's `owner`; any other Pending, a Codex one among them, is refused.
pub(super) fn admits(
    lineage: &Lineage,
    provider: Option<&str>,
    target: &Value,
    owner: Option<&str>,
) -> bool {
    let Lineage::PendingAttached { nonce } = lineage else {
        return false;
    };
    provider == Some("claude")
        && target["execution_nonce"].as_str() == Some(nonce.as_str())
        && owner.is_some_and(|owner| target["owner"].as_str() == Some(owner))
}
