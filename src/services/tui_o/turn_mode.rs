use std::collections::BTreeSet;
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct TurnConfig {
    pub channels: BTreeSet<u64>,
    pub all_owned: bool,
}

impl TurnConfig {
    pub(crate) fn selects(&self, channel: u64) -> bool {
        channel != 0 && (self.all_owned || self.channels.contains(&channel))
    }
}

static CONFIRMED: OnceLock<RwLock<BTreeSet<u64>>> = OnceLock::new();

// Configuration and current output ownership do not confirm a turn transition.
pub(crate) fn transcript_turns(channel: u64) -> bool {
    CONFIRMED.get().is_some_and(|channels| {
        channels
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&channel)
    })
}

// The caller must read the retirement population before confirming a selected channel.
pub(crate) fn confirm(channel: u64) {
    if channel != 0 {
        CONFIRMED
            .get_or_init(|| RwLock::new(BTreeSet::new()))
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(channel);
    }
}

#[cfg(test)]
pub(crate) struct TestConfirmation(u64);

#[cfg(test)]
impl TestConfirmation {
    pub(crate) fn new(channel: u64) -> Self {
        assert!(!transcript_turns(channel));
        confirm(channel);
        Self(channel)
    }
}

#[cfg(test)]
impl Drop for TestConfirmation {
    fn drop(&mut self) {
        CONFIRMED
            .get()
            .unwrap()
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn n1a_configuration_cannot_confirm_a_channel() {
        let config: super::super::shadow::tap::TuiOConfig =
            serde_yaml::from_str("turn: {channels: [63250001, 63250001], all_owned: true}")
                .unwrap();
        assert_eq!(config.turn.channels.len(), 1);
        for channel in [63250001, 63250002] {
            assert!(config.turn.selects(channel));
            assert!(!transcript_turns(channel));
        }
        assert!(!config.turn.selects(0));
        assert_eq!(
            TurnConfig::default(),
            serde_yaml::from_str::<TurnConfig>("{}").unwrap()
        );
        assert!(serde_yaml::from_str::<TurnConfig>("channels: [oops]").is_err());
        assert!(serde_yaml::from_str::<TurnConfig>("unknown: true").is_err());
    }

    #[test]
    fn n1a_output_adoption_and_readiness_do_not_confirm_turn_mode() {
        use crate::services::agent_protocol::RuntimeHandoffKind;
        use crate::services::tui_o::cutover;
        let channel = 63250004;
        let selection = [(channel, RuntimeHandoffKind::ClaudeTui)];
        let pending = cutover::test_override::force_candidates(&selection);
        assert_eq!(
            cutover::peek_o_owns_tui_output_for_channel(
                channel,
                Some(RuntimeHandoffKind::ClaudeTui)
            ),
            Ok(false)
        );
        assert!(!transcript_turns(channel));
        cutover::test_override::with_channels(|snapshot| {
            assert!(snapshot.unwrap().candidate(channel).unwrap().defer(channel));
        });
        assert!(
            !transcript_turns(channel),
            "deferred output adoption is still Legacy"
        );
        drop(pending);
        let committed = cutover::test_override::force_channels(&selection);
        assert_eq!(
            cutover::peek_o_owns_tui_output_for_channel(
                channel,
                Some(RuntimeHandoffKind::ClaudeTui)
            ),
            Ok(true)
        );
        assert!(
            !transcript_turns(channel),
            "output adoption is not turn confirmation"
        );
        let not_ready = cutover::intake_route::test_probe::answers(&[false]);
        assert!(matches!(
            cutover::intake_route::route("claude", channel),
            cutover::intake_route::IntakeRoute::Hold(_)
        ));
        assert!(!transcript_turns(channel));
        drop(not_ready);
        let ready = cutover::intake_route::test_probe::answers(&[true]);
        assert_eq!(
            cutover::intake_route::route("claude", channel),
            cutover::intake_route::IntakeRoute::Gateway
        );
        assert!(
            !transcript_turns(channel),
            "writer readiness is not turn confirmation"
        );
        drop(ready);
        let confirmed = TestConfirmation::new(channel);
        drop(committed);
        assert!(
            transcript_turns(channel),
            "mode reads only the fixed confirmed set"
        );
        drop(confirmed);
    }

    #[test]
    fn n1a_only_confirmation_enables_turn_mode() {
        let channel = 63250003;
        assert!(!transcript_turns(channel));
        let confirmation = TestConfirmation::new(channel);
        assert!(transcript_turns(channel));
        assert!(!transcript_turns(channel + 100));
        drop(confirmation);
        assert!(!transcript_turns(channel));
    }
}
