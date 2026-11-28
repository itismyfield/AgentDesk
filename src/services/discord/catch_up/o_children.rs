//! A thread under a selected O parent that the home never recorded. A Foreign node holds its
//! first input without storing it, so the home's catch-up reads the thread from Discord.

use std::collections::{BTreeMap, BTreeSet};

use poise::serenity_prelude::ChannelId;

use super::CatchUpChannelCandidate;
use super::api::CatchUpDiscordApi;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::{channel_policy::Site, cutover};

/// On the O home, adds each active thread Discord lists under this provider's selected parents.
pub(super) async fn add_unrecorded<A: CatchUpDiscordApi + ?Sized>(
    api: &A,
    provider: &ProviderKind,
    candidates: &mut BTreeMap<u64, CatchUpChannelCandidate>,
) {
    let kind = match provider {
        ProviderKind::Claude => RuntimeHandoffKind::ClaudeTui,
        ProviderKind::Codex => RuntimeHandoffKind::CodexTui,
        _ => return,
    };
    let Some(snapshot) = cutover::boot_snapshot() else {
        return;
    };
    let parents: BTreeSet<u64> = (snapshot.channels().iter().copied())
        .filter(|&parent| snapshot.kind(parent) == Some(kind))
        .collect();
    if *snapshot.site() != Site::Home || parents.is_empty() {
        return;
    }
    for (thread, parent) in api.child_threads(&parents).await {
        candidates
            .entry(thread)
            .or_insert_with(|| CatchUpChannelCandidate {
                channel_id: ChannelId::new(thread),
                fallback_name: None,
                checkpoint_path: None,
                disk_checkpoint: None,
                o_parent: Some(parent),
            });
    }
}
