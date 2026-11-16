//! Engine-owned runtime state shared by messenger adapters.

pub(crate) mod legacy_state;

use std::{ops::Deref, sync::Arc};

use self::legacy_state::LegacyState;
use crate::domain::provider::ProviderKind;

pub(crate) struct PolicyRuntime {
    pub(in crate::services) engine: Option<crate::engine::PolicyEngine>,
}

pub(crate) struct EngineRuntime {
    legacy: LegacyState,
    pub(in crate::services) skills_cache: tokio::sync::RwLock<Vec<(String, String)>>,
    pub(in crate::services) last_turn_at: std::sync::Mutex<Option<String>>,
    pub(in crate::services) provider: ProviderKind,
    pub(in crate::services) api_port: u16,
    pub(in crate::services) pg_pool: Option<sqlx::PgPool>,
    pub(in crate::services) policy: PolicyRuntime,
}

impl EngineRuntime {
    pub(in crate::services) fn new(
        provider: ProviderKind,
        api_port: u16,
        pg_pool: Option<sqlx::PgPool>,
        policy: PolicyRuntime,
        initial_skills: Vec<(String, String)>,
    ) -> Arc<Self> {
        let runtime = Arc::new(Self {
            legacy: LegacyState::default(),
            skills_cache: tokio::sync::RwLock::new(initial_skills),
            last_turn_at: std::sync::Mutex::new(None),
            provider,
            api_port,
            pg_pool,
            policy,
        });
        #[cfg(test)]
        ALLOCATIONS.with(|slot| {
            if let Some(records) = slot.borrow().as_ref() {
                records.borrow_mut().push(Arc::downgrade(&runtime));
            }
        });
        runtime
    }

    #[cfg(test)]
    pub(in crate::services) fn record_allocations_for_tests() -> AllocationRecorder {
        let records: AllocationLog = Default::default();
        ALLOCATIONS.with(|slot| {
            let mut slot = slot.borrow_mut();
            assert!(slot.is_none(), "allocation recording cannot nest");
            *slot = Some(std::rc::Rc::clone(&records));
        });
        AllocationRecorder { records }
    }
}

impl Deref for EngineRuntime {
    type Target = LegacyState;

    fn deref(&self) -> &Self::Target {
        &self.legacy
    }
}

#[cfg(test)]
type AllocationLog = std::rc::Rc<std::cell::RefCell<Vec<std::sync::Weak<EngineRuntime>>>>;

#[cfg(test)]
std::thread_local! {
    static ALLOCATIONS: std::cell::RefCell<Option<AllocationLog>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(in crate::services) struct AllocationRecorder {
    records: AllocationLog,
}

#[cfg(test)]
impl AllocationRecorder {
    pub(in crate::services) fn finish(self) -> Vec<std::sync::Weak<EngineRuntime>> {
        self.records.take()
    }
}

#[cfg(test)]
impl Drop for AllocationRecorder {
    fn drop(&mut self) {
        ALLOCATIONS.with(|slot| drop(slot.take()));
    }
}
