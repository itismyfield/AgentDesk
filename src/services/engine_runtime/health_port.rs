//! Narrow KE ports onto the provider runtimes the Discord health registry holds.
//! KE takes these instead of the registry, which implements them on its side.

mod turn_stop;
pub use turn_stop::{
    FINISH_UNOBSERVED_PATH, InflightDisposition, RuntimeTurnStopResult, TOKEN_SUPERSEDED_PATH,
};
pub(crate) use turn_stop::{PendingQueueView, TurnStopJudgement, TurnStopPort};

mod app_health;
mod headless_turn;
mod queue_cancel;
mod resume_runtime;
mod routine_process;
mod slot_reset;
mod stale_sweep;
pub use app_health::{AppHealth, AppHealthBinding, AppHealthRegistry};
pub(crate) use headless_turn::{
    HeadlessDmTurnStart, HeadlessTurnPort, HeadlessTurnReservationView, HeadlessTurnStart,
};
pub use queue_cancel::FinishCancelledMailboxResult;
pub(crate) use queue_cancel::QueueCancelPort;
pub(crate) use resume_runtime::{ResumePorts, ResumeRuntimePort};
pub(crate) use routine_process::RoutineProcessPort;
pub(crate) use slot_reset::{
    ManagedResetPort, SlotResetOutcome, SlotResetVerdict, SlotRowGuard, SlotRuntimeHealth,
    slot_runtime_health,
};
pub(crate) use stale_sweep::{AxisBSite, StaleSweepPort, stale_sweep_port};
