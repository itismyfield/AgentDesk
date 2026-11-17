#[cfg(test)]
use super::{ChannelId, TmuxWatcherRegistry};
pub(in crate::services::discord) use crate::services::engine_runtime::relay_state::registry::{
    AuthoritativeSessionBinding, TmuxWatcherBinding,
};
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authoritative_snapshot_dedupes_sorts_and_rechecks_live_precedence() {
        let registry =
            TmuxWatcherRegistry::new_with_coords(std::sync::Arc::new(dashmap::DashMap::new()));
        let channel = ChannelId::new(2_201);
        let foreign = ChannelId::new(2_202);
        for name in ["z-last", "a-first", "same", "foreign-live"] {
            registry.seed_binding_for_tests(name, Some(channel), None);
        }
        registry.seed_binding_for_tests("same", None, Some(channel));
        registry.seed_binding_for_tests("foreign-live", None, Some(foreign));
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
