pub mod canary;
pub mod context;
pub mod diagnostics;
pub mod io;
pub mod orchestration;
pub mod paths;
pub mod registry;
pub mod retention;
pub mod session_guard;
pub mod smoke;
pub mod snapshot;
pub mod upgrade;

pub use canary::{AgentInfo, select_canary_agent};
pub use context::ProviderExecutionContext;
pub use diagnostics::{
    MigrationDiagnostics, ProviderCliActionRequest, ProviderCliStatusResponse, ProviderDiagnostics,
    migration_state_wire_value,
};
pub use registry::{LaunchArtifact, ProviderCliChannel, ProviderCliMigrationState};
pub use retention::{build_retention_set, cleanup_dry_run};
