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
    DiagnosticsSnapshot, MigrationDiagnostics, ProviderCliActionRequest, ProviderCliStatusResponse,
    ProviderDiagnostics, RuntimeConsistency, SessionDiagnostics, build_snapshot,
    migration_state_wire_value,
};
pub use registry::{
    LaunchArtifact, MigrationHistoryEntry, MigrationState, PROVIDER_UPDATE_STRATEGIES,
    ProviderChannels, ProviderCliChannel, ProviderCliMigrationState, ProviderCliRegistry,
    ProviderCliUpdateStrategy, SmokeCheckStatus, SmokeChecks, SmokeResult, update_strategy_for,
};
pub use retention::{RetentionSet, build_retention_set, cleanup_dry_run};
pub use smoke::{run_smoke, smoke_passed};
pub use snapshot::snapshot_current_channel;
pub use upgrade::{UpgradeError, UpgradeResult, new_migration_state, run_upgrade, transition};
