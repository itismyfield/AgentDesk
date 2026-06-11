use std::fmt;

use crate::config::RoutinesConfig;

use super::store::ROUTINE_RUN_LEASE_SECS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineRuntimeConfigError {
    TickIntervalSecs,
    AgentPollLimit,
    AgentTimeoutSecs,
    MaxCheckpointBytes,
}

impl RoutineRuntimeConfigError {
    pub const fn message(self) -> &'static str {
        match self {
            Self::TickIntervalSecs => {
                "routines.tick_interval_secs must be greater than zero and no more than half the routine run lease window"
            }
            Self::AgentPollLimit => "routines.max_agent_polls_per_tick must be greater than zero",
            Self::AgentTimeoutSecs => "routines.agent_timeout_secs must be greater than zero",
            Self::MaxCheckpointBytes => "routines.max_checkpoint_bytes must be greater than zero",
        }
    }
}

impl fmt::Display for RoutineRuntimeConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

pub fn validate_routine_runtime_config(
    config: &RoutinesConfig,
) -> Result<u64, RoutineRuntimeConfigError> {
    valid_routine_tick_interval_secs(config.tick_interval_secs)
        .ok_or(RoutineRuntimeConfigError::TickIntervalSecs)?;
    valid_routine_agent_poll_limit(config.max_agent_polls_per_tick)
        .ok_or(RoutineRuntimeConfigError::AgentPollLimit)?;
    valid_routine_agent_timeout_secs(config.agent_timeout_secs)
        .ok_or(RoutineRuntimeConfigError::AgentTimeoutSecs)?;
    valid_routine_max_checkpoint_bytes(config.max_checkpoint_bytes)
        .ok_or(RoutineRuntimeConfigError::MaxCheckpointBytes)?;
    Ok(config.tick_interval_secs)
}

pub fn valid_routine_tick_interval_secs(value: u64) -> Option<u64> {
    let max_safe_tick_secs = ROUTINE_RUN_LEASE_SECS / 2;
    (value > 0 && value <= max_safe_tick_secs).then_some(value)
}

pub fn valid_routine_agent_poll_limit(value: u32) -> Option<u32> {
    (value > 0).then_some(value)
}

pub fn valid_routine_agent_timeout_secs(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

pub fn valid_routine_max_checkpoint_bytes(value: usize) -> Option<usize> {
    (value > 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routine_tick_interval_rejects_zero_and_over_half_lease() {
        assert_eq!(valid_routine_tick_interval_secs(0), None);
        assert_eq!(valid_routine_tick_interval_secs(1), Some(1));
        assert_eq!(valid_routine_tick_interval_secs(30), Some(30));
        assert_eq!(valid_routine_tick_interval_secs(900), Some(900));
        assert_eq!(valid_routine_tick_interval_secs(901), None);
        assert_eq!(valid_routine_tick_interval_secs(1800), None);
    }

    #[test]
    fn routine_agent_poll_limit_rejects_zero() {
        assert_eq!(valid_routine_agent_poll_limit(0), None);
        assert_eq!(valid_routine_agent_poll_limit(1), Some(1));
        assert_eq!(valid_routine_agent_poll_limit(10), Some(10));
    }

    #[test]
    fn routine_agent_timeout_rejects_zero() {
        assert_eq!(valid_routine_agent_timeout_secs(0), None);
        assert_eq!(valid_routine_agent_timeout_secs(1), Some(1));
        assert_eq!(valid_routine_agent_timeout_secs(1800), Some(1800));
    }

    #[test]
    fn routine_max_checkpoint_bytes_rejects_zero() {
        assert_eq!(valid_routine_max_checkpoint_bytes(0), None);
        assert_eq!(valid_routine_max_checkpoint_bytes(1), Some(1));
        assert_eq!(
            valid_routine_max_checkpoint_bytes(256 * 1024),
            Some(256 * 1024)
        );
    }

    #[test]
    fn routine_runtime_config_returns_first_invalid_field() {
        let mut config = RoutinesConfig::default();
        config.tick_interval_secs = 901;
        config.max_agent_polls_per_tick = 0;
        assert_eq!(
            validate_routine_runtime_config(&config),
            Err(RoutineRuntimeConfigError::TickIntervalSecs)
        );

        config.tick_interval_secs = 30;
        assert_eq!(
            validate_routine_runtime_config(&config),
            Err(RoutineRuntimeConfigError::AgentPollLimit)
        );

        config.max_agent_polls_per_tick = 10;
        config.agent_timeout_secs = 0;
        assert_eq!(
            validate_routine_runtime_config(&config),
            Err(RoutineRuntimeConfigError::AgentTimeoutSecs)
        );

        config.agent_timeout_secs = 1800;
        config.max_checkpoint_bytes = 0;
        assert_eq!(
            validate_routine_runtime_config(&config),
            Err(RoutineRuntimeConfigError::MaxCheckpointBytes)
        );
    }
}
