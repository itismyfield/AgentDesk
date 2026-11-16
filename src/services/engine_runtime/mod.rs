//! Engine-owned runtime state shared by messenger adapters.

pub(crate) mod legacy_state;

use std::ops::Deref;

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
    ) -> Self {
        Self {
            legacy: LegacyState::default(),
            skills_cache: tokio::sync::RwLock::new(initial_skills),
            last_turn_at: std::sync::Mutex::new(None),
            provider,
            api_port,
            pg_pool,
            policy,
        }
    }
}

impl Deref for EngineRuntime {
    type Target = LegacyState;

    fn deref(&self) -> &Self::Target {
        &self.legacy
    }
}
