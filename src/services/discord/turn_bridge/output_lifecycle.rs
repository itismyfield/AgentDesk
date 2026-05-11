#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BridgeOutputOwner {
    /// Watcher already owns live assistant relay for this turn; bridge must not
    /// duplicate the terminal response.
    WatcherRelay,
    /// Standby relay owns the JSONL -> Discord delivery path for this turn.
    StandbyRelay,
}

pub(super) fn classify_bridge_output_owner(
    standby_relay_owns_output: bool,
    bridge_relay_delegated_to_watcher: bool,
) -> Option<BridgeOutputOwner> {
    if bridge_relay_delegated_to_watcher {
        Some(BridgeOutputOwner::WatcherRelay)
    } else if standby_relay_owns_output {
        Some(BridgeOutputOwner::StandbyRelay)
    } else {
        None
    }
}

impl BridgeOutputOwner {
    pub(super) fn skips_bridge_spinner_cleanup(self) -> bool {
        matches!(
            self,
            BridgeOutputOwner::WatcherRelay | BridgeOutputOwner::StandbyRelay
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_output_owner_is_absent_for_bridge_owned_delivery() {
        assert_eq!(
            classify_bridge_output_owner(false, false),
            None,
            "bridge-owned turns are represented by no external output owner"
        );
    }

    #[test]
    fn bridge_disconnect_handoff_flags_do_not_create_legacy_owner() {
        let bridge_response_empty = true;
        let rx_disconnected = true;
        let tmux_handed_off = true;

        assert!(bridge_response_empty && rx_disconnected && tmux_handed_off);
        assert_eq!(
            classify_bridge_output_owner(false, false),
            None,
            "legacy bridge-to-watcher handoff flags must not create an output owner"
        );
    }

    #[test]
    fn bridge_output_owner_characterizes_standby_relay_contract() {
        let owner = classify_bridge_output_owner(true, false).expect("standby owner");
        assert_eq!(owner, BridgeOutputOwner::StandbyRelay);
        assert!(
            owner.skips_bridge_spinner_cleanup(),
            "standby relay owns visible output delivery"
        );
    }

    #[test]
    fn bridge_output_owner_prefers_explicit_watcher_relay_delegation() {
        let owner = classify_bridge_output_owner(true, true).expect("watcher owner");
        assert_eq!(owner, BridgeOutputOwner::WatcherRelay);
        assert!(
            owner.skips_bridge_spinner_cleanup(),
            "watcher relay delegation must not duplicate bridge terminal delivery"
        );
    }
}
