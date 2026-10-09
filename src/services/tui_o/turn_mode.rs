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

// Dormant effect gate: execute only the synchronous first HTTP poll under the confirmation lock.
// Await the remaining HTTP work after this function returns.
pub(crate) fn admit_effect<T>(
    channel: u64,
    transcript: bool,
    hand_off: impl FnOnce() -> T,
) -> Option<T> {
    if channel == 0 {
        return None;
    }
    let channels = CONFIRMED
        .get_or_init(|| RwLock::new(BTreeSet::new()))
        .read()
        .unwrap_or_else(|e| e.into_inner());
    (channels.contains(&channel) == transcript).then(hand_off)
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

/// Confirms each selected channel of `owned` only after `retire` reports it fully retired. An empty
/// selection returns before `owned` or `retire` runs, so an unconfigured process reads nothing.
pub(crate) fn confirm_selected(
    config: Option<&TurnConfig>,
    owned: impl FnOnce() -> Vec<u64>,
    mut retire: impl FnMut(u64) -> bool,
) -> Vec<u64> {
    let Some(config) = config.filter(|c| c.all_owned || !c.channels.is_empty()) else {
        return Vec::new();
    };
    let mut confirmed = Vec::new();
    for channel in owned().into_iter().filter(|&c| config.selects(c)) {
        if retire(channel) {
            confirm(channel);
            confirmed.push(channel);
        }
    }
    confirmed
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

    /// Takes over removal of a channel production code already confirmed.
    pub(crate) fn confirmed(channel: u64) -> Self {
        assert!(transcript_turns(channel));
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

    #[test]
    fn b1_effect_admission_rejects_mismatched_modes() {
        let channel = 63250061;
        assert_eq!(admit_effect(channel, false, || 7), Some(7));
        assert_eq!(
            admit_effect(channel, true, || panic!("transcript before confirm")),
            None
        );
        let confirmation = TestConfirmation::new(channel);
        assert_eq!(
            admit_effect(channel, false, || panic!("native after confirm")),
            None
        );
        assert_eq!(admit_effect(channel, true, || 8), Some(8));
        drop(confirmation);
    }

    #[test]
    fn b1_effect_admission_rejects_zero_without_callback() {
        for transcript in [false, true] {
            assert_eq!(
                admit_effect(0, transcript, || panic!("zero has no effect authority")),
                None
            );
        }
    }

    #[test]
    fn b1_native_admission_does_not_confirm_turn_mode() {
        let channel = 63250062;
        assert!(!transcript_turns(channel));
        assert_eq!(admit_effect(channel, false, || "native"), Some("native"));
        assert!(!transcript_turns(channel));
    }

    #[test]
    fn b1_confirmation_waits_for_native_effect_handoff() {
        use std::sync::mpsc;
        use std::time::Duration;

        let channel = 63250063;
        let timeout = Duration::from_secs(5);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let effect = std::thread::spawn(move || {
            admit_effect(channel, false, || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(timeout).unwrap();
            })
        });
        entered_rx.recv_timeout(timeout).unwrap();
        assert!(
            CONFIRMED.get().unwrap().try_write().is_err(),
            "handoff must hold the read lock"
        );

        let (started_tx, started_rx) = mpsc::channel();
        let (committed_tx, committed_rx) = mpsc::channel();
        let confirmation = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            confirm(channel);
            committed_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(timeout).unwrap();
        assert_eq!(committed_rx.try_recv(), Err(mpsc::TryRecvError::Empty));
        release_tx.send(()).unwrap();
        committed_rx.recv_timeout(timeout).unwrap();
        assert_eq!(effect.join().unwrap(), Some(()));
        confirmation.join().unwrap();
        let confirmed = TestConfirmation::confirmed(channel);
        assert_eq!(
            admit_effect(channel, false, || panic!("commit blocks native")),
            None
        );
        assert_eq!(admit_effect(channel, true, || ()), Some(()));
        drop(confirmed);
    }
}

#[cfg(test)]
pub(crate) mod test_tick {
    static TICK: std::sync::Mutex<Option<(u64, tokio::sync::oneshot::Sender<()>)>> =
        std::sync::Mutex::new(None);

    pub(crate) fn signal(channel: u64) -> tokio::sync::oneshot::Receiver<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        *TICK.lock().unwrap_or_else(|e| e.into_inner()) = Some((channel, tx));
        rx
    }

    pub(crate) fn completed(channel: u64) {
        let mut tick = TICK.lock().unwrap_or_else(|e| e.into_inner());
        if tick.as_ref().is_some_and(|(id, _)| *id == channel) {
            let (_, tx) = tick.take().unwrap();
            let _ = tx.send(());
        }
    }
}
