//! Rigs whose TUI channel is already O's: under the hold contract Legacy sends no TUI body, so a
//! turn there ends only through O's delegated delivery, which needs a committed adoption.

use std::path::Path;

/// Writes an O store era over `channels` under `root`, so a boot policy installed later seeds each
/// channel Committed instead of an adoption no writer host here would ever finish.
pub(crate) fn commit_o_store(root: &Path, channels: &[u64]) {
    use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root)
        .expect("open the O store")
        .expect("an enabled O store");
    let at = chrono::Utc::now();
    let init = |channel| {
        let (sources, initial_anchor, build_digest) = (Vec::new(), 0, "test".into());
        Ok(Initialized {
            channel,
            sources,
            initial_anchor,
            build_digest,
            at,
        })
    };
    store.begin_era(channels, at, init).expect("an O store era");
}

/// Commits the installed boot policy's adoption of `channel`, as O's writer would once its init is
/// readable, for a rig whose turn was admitted unadopted and must end through O's delivery.
pub(crate) fn adopt_installed(channel: u64) {
    let candidate = crate::services::tui_o::channel_policy::boot()
        .and_then(|boot| boot.candidate(channel).cloned())
        .expect("the installed candidate");
    assert!(candidate.confirm_store(), "the adoption commits");
}
