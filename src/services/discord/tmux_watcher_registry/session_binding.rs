use std::collections::BTreeSet;

use super::{ChannelId, TmuxWatcherRegistry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct TmuxWatcherBinding {
    pub(in crate::services::discord) owner_channel_id: ChannelId,
    pub(in crate::services::discord) tmux_session_name: String,
    // Cloned from the live handle; concurrent teardown can leave this snapshot without a path.
    pub(in crate::services::discord) output_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct AuthoritativeSessionBinding {
    pub owner_channel_id: ChannelId,
    pub tmux_session_name: String,
}

impl TmuxWatcherRegistry {
    pub(in crate::services::discord) fn authoritative_session_bindings(
        &self,
        owner_channel_id: ChannelId,
    ) -> Vec<AuthoritativeSessionBinding> {
        let mut names = BTreeSet::new();
        for owners in [
            &self.owner_channel_by_tmux_session,
            &self.restored_owner_by_tmux_session,
        ] {
            #[cfg(test)]
            if tests::skip_map(owners, &self.restored_owner_by_tmux_session) {
                continue;
            }
            names.extend(
                owners
                    .iter()
                    .filter(|entry| *entry.value() == owner_channel_id)
                    .map(|entry| entry.key().clone()),
            );
        }
        names
            .into_iter()
            .filter_map(|tmux_session_name| {
                (self.owner_channel_for_tmux_session(&tmux_session_name) == Some(owner_channel_id))
                    .then_some(AuthoritativeSessionBinding {
                        owner_channel_id,
                        tmux_session_name,
                    })
            })
            .collect()
    }

    pub(in crate::services::discord) fn channel_binding(
        &self,
        channel_id: &ChannelId,
    ) -> Option<TmuxWatcherBinding> {
        #[cfg(test)]
        if let Some(binding) = tests::widen_binding(self, channel_id) {
            return Some(binding);
        }
        let tmux_session_name = self.tmux_session_by_channel.get(channel_id)?.clone();
        let owner_channel_id = self
            .owner_channel_by_tmux_session
            .get(&tmux_session_name)
            .map(|entry| *entry.value())
            .unwrap_or(*channel_id);
        let output_path = self.watcher_output_path(&tmux_session_name);
        Some(TmuxWatcherBinding {
            owner_channel_id,
            tmux_session_name,
            output_path,
        })
    }

    pub(in crate::services::discord) fn owner_channel_for_tmux_session(
        &self,
        tmux_session_name: &str,
    ) -> Option<ChannelId> {
        // A live claim wins over restored ownership; the TTL mirror has no authority.
        self.owner_channel_by_tmux_session
            .get(tmux_session_name)
            .map(|entry| *entry.value())
            .or_else(|| {
                self.restored_owner_by_tmux_session
                    .get(tmux_session_name)
                    .map(|entry| *entry.value())
            })
    }
}

#[cfg(all(test, unix))]
pub(in crate::services::discord) use tests::owner_stop_mutant;

#[cfg(test)]
mod tests {
    use super::*;

    pub(in crate::services::discord) fn owner_stop_mutant(name: &str) -> bool {
        std::env::var("ADK_OWNER_STOP_MUTANT").as_deref() == Ok(name)
    }

    pub(super) fn skip_map(
        owners: &dashmap::DashMap<String, ChannelId>,
        restored: &dashmap::DashMap<String, ChannelId>,
    ) -> bool {
        owner_stop_mutant("2a-live-only") && std::ptr::eq(owners, restored)
    }

    pub(super) fn widen_binding(
        registry: &TmuxWatcherRegistry,
        channel: &ChannelId,
    ) -> Option<TmuxWatcherBinding> {
        if !owner_stop_mutant("2a-channel-binding-widened") {
            return None;
        }
        let owner = registry
            .authoritative_session_bindings(*channel)
            .into_iter()
            .next()?;
        Some(TmuxWatcherBinding {
            owner_channel_id: owner.owner_channel_id,
            tmux_session_name: owner.tmux_session_name,
            output_path: None,
        })
    }

    #[test]
    fn authoritative_snapshot_dedupes_sorts_and_rechecks_live_precedence() {
        let registry =
            TmuxWatcherRegistry::new_with_coords(std::sync::Arc::new(dashmap::DashMap::new()));
        let channel = ChannelId::new(2_201);
        let foreign = ChannelId::new(2_202);
        for name in ["z-last", "a-first", "same", "foreign-live"] {
            registry
                .restored_owner_by_tmux_session
                .insert(name.into(), channel);
        }
        registry
            .owner_channel_by_tmux_session
            .insert("same".into(), channel);
        registry
            .owner_channel_by_tmux_session
            .insert("foreign-live".into(), foreign);
        assert_eq!(
            registry.authoritative_session_bindings(channel),
            ["a-first", "same", "z-last"].map(|name| AuthoritativeSessionBinding {
                owner_channel_id: channel,
                tmux_session_name: name.into()
            })
        );
        assert!(registry.channel_binding(&channel).is_none());
        registry.clear_restored_owner_for_tmux_session("a-first");
        assert_eq!(registry.authoritative_session_bindings(channel).len(), 2);
    }
}
