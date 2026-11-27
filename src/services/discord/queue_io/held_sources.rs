use super::super::recovery_known_ids::RecoveryKnownIdArm;

/// Ids a read-only V2 queue file keeps stay known, so catch-up never takes them again on a binary
/// that cannot write that file; only a settled one may let the frontier pass.
pub(in crate::services::discord) async fn merge_v2_held_sources(
    shared: &crate::services::discord::SharedData,
    channel: poise::serenity_prelude::ChannelId,
    arms: &mut std::collections::HashMap<u64, RecoveryKnownIdArm>,
    ids: &mut std::collections::HashSet<u64>,
) {
    // An input owner holding the file admits by ticket instead.
    #[cfg(test)]
    if shared
        .mailbox(channel)
        .v2_request(|slot| Ok(slot.is_some()))
        .await
        == Ok(true)
    {
        return;
    }
    use RecoveryKnownIdArm::{Queued, V2Delivered};
    let held = crate::services::turn_orchestrator::v2_held_sources;
    let held = held(&shared.provider, &shared.token_hash, channel);
    let owed = held.owed.into_iter().map(|id| (id, Queued));
    let delivered = held.delivered.into_iter().map(|id| (id, V2Delivered));
    for (id, arm) in owed.chain(delivered) {
        arms.entry(id).or_insert(arm);
        ids.insert(id);
    }
}
