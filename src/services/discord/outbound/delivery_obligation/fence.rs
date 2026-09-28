use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::services::provider::ProviderKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::services::discord) struct LedgerFence {
    pub rev: u64,
}

pub(in crate::services::discord) fn fence_path(
    runtime_root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
) -> PathBuf {
    runtime_root
        .join("discord_delivery_obligation_fence")
        .join(provider.as_str())
        .join(format!("{channel_id}.json"))
}
