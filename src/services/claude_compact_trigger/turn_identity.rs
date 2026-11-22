//! The live turn a compact request is bound to, as the trigger reads it.

/// A captured compact-eligible turn; its owner rechecks it against the live turn before a send.
pub(in crate::services) trait CompactTurnIdentity:
    Send + Sync + 'static
{
    fn channel_id(&self) -> u64;
    fn tmux_session_name(&self) -> &str;
    /// A captured eligible turn can finish before compact; a successor or failed read cannot
    /// approve it.
    fn pending_turn_matches(&self) -> Result<bool, String>;
}
