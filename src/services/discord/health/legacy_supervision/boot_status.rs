use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum BootPhase {
    #[default]
    Collecting,
    Confirming,
    Released,
    Held,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum BootSlotState {
    Preparing,
    Reaped,
    ExcludedNoRuntime,
    Failed,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BootRetirementHealth {
    pub phase: BootPhase,
    pub elapsed_ms: u64,
    pub timed_out: bool,
    pub expected: usize,
    pub reaped: usize,
    pub excluded: usize,
    pub failed: usize,
    pub waiting_bots: Vec<String>,
    pub waiting_providers: Vec<String>,
    pub completed_providers: Vec<String>,
    pub published_keys: Vec<(String, u64)>,
    pub refused_keys: Vec<((String, u64), String)>,
    pub failure: Option<String>,
    pub supervisors_released: bool,
}
