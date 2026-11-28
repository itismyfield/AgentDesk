use std::sync::{Arc, OnceLock};

use crate::engine::ops::turn_ops::handoff_port::AgentHandoffPort;

use super::OwnerInputRuntimePort;

pub(crate) trait RuntimeRegistryPort: AgentHandoffPort + OwnerInputRuntimePort {}

type RegistryAccessor = fn() -> Option<Arc<dyn RuntimeRegistryPort>>;
static REGISTRY_ACCESSOR: OnceLock<RegistryAccessor> = OnceLock::new();

pub(crate) fn install_runtime_registry_accessor(accessor: RegistryAccessor) {
    REGISTRY_ACCESSOR.get_or_init(|| accessor);
}

pub(crate) fn published_runtime_registry() -> Option<Arc<dyn RuntimeRegistryPort>> {
    REGISTRY_ACCESSOR.get()?()
}
