use super::{ProviderKind, TurnKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum ClaimRejection {
    IdentityMismatch,
    NativeControlCustodyDenied,
    ActorMiss,
}

impl ClaimRejection {
    pub(super) fn warn(
        self,
        provider: &ProviderKind,
        key: TurnKey,
        captured_nonce: Option<&str>,
        observed_nonce: Option<&str>,
    ) {
        tracing::warn!(
            provider = provider.as_str(),
            channel_id = key.channel_id.get(),
            turn_nonce = captured_nonce.or(observed_nonce),
            observed_turn_nonce = observed_nonce,
            user_msg_id = key.user_msg_id,
            reason = ?self,
            "episode_claim_rejected"
        );
    }
}
