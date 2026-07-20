//! Per-session generation slots for cancellation cleanup authority.

use super::super::{ProviderKind, parse_provider_and_channel_from_tmux_name};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SessionKey {
    provider: ProviderKind,
    name: String,
}

impl SessionKey {
    pub(crate) fn new(provider: ProviderKind, name: impl Into<String>) -> Self {
        Self {
            provider,
            name: name.into(),
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TmuxBinding {
    Published { key: SessionKey, generation: u64 },
    NameOnly { name: String },
}

impl TmuxBinding {
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Published { key, .. } => key.name(),
            Self::NameOnly { name } => name,
        }
    }
}

#[derive(Debug)]
struct SessionSlot {
    current_generation: u64,
}

static SESSION_SLOTS: LazyLock<Mutex<HashMap<SessionKey, Arc<Mutex<SessionSlot>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A current-generation lease. Destructive callers must retain it through I/O.
pub(crate) struct SessionKillGuard {
    guard: MutexGuard<'static, SessionSlot>,
    _slot: Arc<Mutex<SessionSlot>>,
}

impl SessionKillGuard {
    pub(crate) fn registry_generation(&self) -> u64 {
        self.guard.current_generation
    }
}

pub(crate) enum KillAuthorization {
    Unregistered,
    Current(SessionKillGuard),
    Stale {
        token_generation: u64,
        registry_generation: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KillAuthorizationState {
    Unregistered,
    Current,
    Stale,
    Duplicate,
}

fn session_slot(key: &SessionKey, initial_generation: u64) -> Arc<Mutex<SessionSlot>> {
    SESSION_SLOTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .entry(key.clone())
        .or_insert_with(|| {
            Arc::new(Mutex::new(SessionSlot {
                current_generation: initial_generation,
            }))
        })
        .clone()
}

pub(crate) fn publish(provider: ProviderKind, name: &str, generation: u64) -> Option<TmuxBinding> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    // #4593 S1: a name/provider mismatch is tolerated in production (warn + proceed
    // below), so a debug_assert here would diverge debug/test behavior from the
    // production contract and panic legitimate callers (e.g. wrapper-interrupt fencing
    // tests whose session name does not parse to the published provider).
    let parsed_provider = parse_provider_and_channel_from_tmux_name(name).map(|(kind, _)| kind);
    if parsed_provider.as_ref() != Some(&provider) {
        tracing::warn!(
            provider = provider.as_str(),
            tmux_session = name,
            "cancel cleanup bind provider does not match tmux session name"
        );
    }

    let key = SessionKey::new(provider, name);
    let slot = session_slot(&key, generation);
    let mut slot_guard = slot.lock().unwrap_or_else(|error| error.into_inner());
    slot_guard.current_generation = slot_guard.current_generation.max(generation);
    drop(slot_guard);

    Some(TmuxBinding::Published { key, generation })
}

pub(crate) fn authorize_state(
    binding: Option<&TmuxBinding>,
    duplicate: bool,
) -> KillAuthorizationState {
    if duplicate {
        return KillAuthorizationState::Duplicate;
    }
    match authorize(binding) {
        KillAuthorization::Unregistered => KillAuthorizationState::Unregistered,
        KillAuthorization::Current(_) => KillAuthorizationState::Current,
        KillAuthorization::Stale { .. } => KillAuthorizationState::Stale,
    }
}

pub(crate) fn authorize(binding: Option<&TmuxBinding>) -> KillAuthorization {
    let Some(TmuxBinding::Published { key, generation }) = binding else {
        return KillAuthorization::Unregistered;
    };

    let slot = session_slot(key, *generation);
    let guard = slot.lock().unwrap_or_else(|error| error.into_inner());
    let guard = unsafe {
        // SAFETY: SessionKillGuard stores the Arc that owns this mutex before the
        // guard and therefore keeps the mutex alive until after the guard drops.
        std::mem::transmute::<MutexGuard<'_, SessionSlot>, MutexGuard<'static, SessionSlot>>(guard)
    };
    let registry_generation = guard.current_generation;
    if registry_generation == *generation {
        KillAuthorization::Current(SessionKillGuard { guard, _slot: slot })
    } else {
        drop(guard);
        KillAuthorization::Stale {
            token_generation: *generation,
            registry_generation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    fn claude_key(name: &str) -> SessionKey {
        SessionKey::new(ProviderKind::Claude, name)
    }

    #[test]
    fn old_generation_is_stale_after_new_generation_publishes() {
        let name = "AgentDesk-claude-authority-stale";
        let old = publish(ProviderKind::Claude, name, 10).unwrap();
        publish(ProviderKind::Claude, name, 11).unwrap();

        assert!(matches!(
            authorize(Some(&old)),
            KillAuthorization::Stale {
                token_generation: 10,
                registry_generation: 11
            }
        ));
    }

    #[test]
    fn current_guard_blocks_new_publish_until_release() {
        let name = "AgentDesk-claude-authority-serialized";
        let current = publish(ProviderKind::Claude, name, 20).unwrap();
        let authorization = authorize(Some(&current));
        let KillAuthorization::Current(guard) = authorization else {
            panic!("current generation must acquire its slot");
        };
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let name = name.to_string();
        let publisher = thread::spawn(move || {
            started_tx.send(()).unwrap();
            publish(ProviderKind::Claude, &name, 21);
            done_tx.send(()).unwrap();
        });

        started_rx.recv().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(guard);
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        publisher.join().unwrap();
    }

    #[test]
    fn provider_is_part_of_session_key() {
        let name = "AgentDesk-claude-authority-provider-key";
        assert_ne!(claude_key(name), SessionKey::new(ProviderKind::Codex, name));
    }

    #[test]
    fn name_only_and_missing_bindings_are_unregistered() {
        let name_only = TmuxBinding::NameOnly {
            name: "AgentDesk-codex-authority-name-only".to_string(),
        };
        assert!(matches!(
            authorize(Some(&name_only)),
            KillAuthorization::Unregistered
        ));
        assert!(matches!(authorize(None), KillAuthorization::Unregistered));
    }

    #[test]
    fn authorization_state_exposes_all_four_kill_outcomes() {
        let name = "AgentDesk-claude-authority-four-states";
        let current = publish(ProviderKind::Claude, name, 40).unwrap();
        let stale = publish(ProviderKind::Claude, name, 39).unwrap();

        assert_eq!(
            authorize_state(None, false),
            KillAuthorizationState::Unregistered
        );
        assert_eq!(
            authorize_state(Some(&current), false),
            KillAuthorizationState::Current
        );
        assert_eq!(
            authorize_state(Some(&stale), false),
            KillAuthorizationState::Stale
        );
        assert_eq!(
            authorize_state(Some(&current), true),
            KillAuthorizationState::Duplicate
        );
    }
}
