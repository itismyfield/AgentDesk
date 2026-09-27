//! Read-only binding access for observers that must neither purge relay state nor wait on it.

use super::*;

/// Copies a live runtime binding without purging expired entries; `None` when the lock is busy.
pub(crate) fn peek_tmux_runtime_binding(tmux_session_name: &str) -> Option<TuiRuntimeBinding> {
    let state = match STATE.try_lock() {
        Ok(state) => state,
        Err(std::sync::TryLockError::Poisoned(poison)) => poison.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return None,
    };
    let entry = state.runtime_by_tmux.get(tmux_session_name.trim())?;
    (entry.recorded_at.elapsed() <= SESSION_MAPPING_TTL).then(|| entry.value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tui_o::shadow::binding_reader::{BindingLookup, LiveBindingLookup};

    #[test]
    fn shadow_lookup_copies_live_bindings_without_purging_relay_state() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        reset_state_for_tests();
        let expired = Instant::now() - SESSION_MAPPING_TTL - Duration::from_secs(1);
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
}
