//! Read-only binding access for observers that must neither purge relay state nor wait on it.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BindingPeekBusy;

/// Copies a live binding without purging state; lock contention is distinct from absence.
pub(crate) fn try_peek_tmux_runtime_binding(
    tmux_session_name: &str,
) -> Result<Option<TuiRuntimeBinding>, BindingPeekBusy> {
    let state = match STATE.try_lock() {
        Ok(state) => state,
        Err(std::sync::TryLockError::Poisoned(poison)) => poison.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return Err(BindingPeekBusy),
    };
    Ok(state
        .runtime_by_tmux
        .get(tmux_session_name.trim())
        .filter(|entry| entry.recorded_at.elapsed() <= SESSION_MAPPING_TTL)
        .map(|entry| entry.value.clone()))
}

/// Preserves the observer's optional lookup when the shared state cannot be locked.
pub(crate) fn peek_tmux_runtime_binding(tmux_session_name: &str) -> Option<TuiRuntimeBinding> {
    try_peek_tmux_runtime_binding(tmux_session_name)
        .ok()
        .flatten()
}

#[cfg(test)]
pub(crate) fn hold_binding_peek_lock_for_tests() -> impl Drop {
    STATE.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tui_o::shadow::binding_reader::{BindingLookup, LiveBindingLookup};

    #[test]
    fn existing_binding_peek_semantics_are_preserved() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        reset_state_for_tests();
        let expired = {
            let mut state = STATE.lock().unwrap_or_else(|p| p.into_inner());
            state.aged_stamp_for_tests(SESSION_MAPPING_TTL + Duration::from_secs(1))
        };
        let victim = PromptKey::new("claude", "shadow-peek-victim");
        let binding = TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/t/s.jsonl".into(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        };
        {
            let mut state = STATE.lock().unwrap_or_else(|poison| poison.into_inner());
            let lease = ExternalInputRelayLease::unassigned(Some(42));
            let lease = TimedValue {
                value: lease,
                recorded_at: expired,
            };
            state
                .external_input_relay_lease_by_tmux
                .insert(victim.clone(), lease);
            let entry = TimedValue {
                value: "entry".to_string(),
                recorded_at: expired,
            };
            state
                .relayed_entry_ids_by_tmux
                .insert(victim.clone(), VecDeque::from([entry]));
            for (name, at) in [
                ("shadow-peek-live", Instant::now()),
                ("shadow-peek-stale", expired),
            ] {
                let value = TimedValue {
                    value: binding.clone(),
                    recorded_at: at,
                };
                state.runtime_by_tmux.insert(name.to_string(), value);
            }
        }

        let live = LiveBindingLookup.lookup("shadow-peek-live");
        assert_eq!(
            live.map(|view| view.output_path),
            Some("/t/s.jsonl".to_string())
        );
        assert!(LiveBindingLookup.lookup("shadow-peek-stale").is_none());
        assert!(LiveBindingLookup.lookup("shadow-peek-absent").is_none());
        for (name, expected) in [
            (" shadow-peek-live ", Some(binding)),
            ("shadow-peek-stale", None),
            ("shadow-peek-absent", None),
        ] {
            assert_eq!(try_peek_tmux_runtime_binding(name), Ok(expected.clone()));
            assert_eq!(peek_tmux_runtime_binding(name), expected);
        }
        let state = STATE.lock().unwrap_or_else(|poison| poison.into_inner());
        let kept = (
            state
                .external_input_relay_lease_by_tmux
                .contains_key(&victim),
            state.relayed_entry_ids_by_tmux.contains_key(&victim),
            state.runtime_by_tmux.contains_key("shadow-peek-stale"),
        );
        drop(state);
        reset_state_for_tests();
        assert_eq!(
            kept,
            (true, true, true),
            "a shadow lookup must not purge relay state"
        );
    }

    #[test]
    fn detailed_binding_peek_distinguishes_busy_without_mutating_state() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        reset_state_for_tests();
        let mut state = STATE.lock().unwrap_or_else(|poison| poison.into_inner());
        let expired = state.aged_stamp_for_tests(SESSION_MAPPING_TTL + Duration::from_secs(1));
        state.channel_by_tmux.insert(
            "busy-peek-stale".to_string(),
            TimedValue {
                value: 42,
                recorded_at: expired,
            },
        );
        assert_eq!(
            try_peek_tmux_runtime_binding("busy-peek-absent"),
            Err(BindingPeekBusy)
        );
        assert_eq!(peek_tmux_runtime_binding("busy-peek-absent"), None);
        assert!(state.channel_by_tmux.contains_key("busy-peek-stale"));
        drop(state);
        assert_eq!(try_peek_tmux_runtime_binding("busy-peek-absent"), Ok(None));
        reset_state_for_tests();
    }

    #[test]
    fn detailed_binding_peek_recovers_poison_with_existing_optional_semantics() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        reset_state_for_tests();
        let binding = TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/t/poison.jsonl".into(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        };
        let poisoned = std::panic::catch_unwind(|| {
            let mut state = STATE.lock().unwrap_or_else(|poison| poison.into_inner());
            state.runtime_by_tmux.insert(
                "poison-peek-live".to_string(),
                TimedValue {
                    value: binding.clone(),
                    recorded_at: Instant::now(),
                },
            );
            panic!("poison binding state for lookup recovery");
        });
        let detailed = try_peek_tmux_runtime_binding("poison-peek-live");
        let optional = peek_tmux_runtime_binding("poison-peek-live");
        let absent = try_peek_tmux_runtime_binding("poison-peek-absent");
        STATE.clear_poison();
        reset_state_for_tests();
        assert!(poisoned.is_err());
        assert_eq!(detailed, Ok(Some(binding.clone())));
        assert_eq!(optional, Some(binding));
        assert_eq!(absent, Ok(None));
    }
}
