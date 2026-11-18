use std::sync::atomic::Ordering;

use sqlx::PgPool;

#[derive(Clone, Copy, Debug)]
pub(crate) struct PressureSnapshot {
    pub(crate) pool_size: u32,
    pub(crate) pool_idle: u32,
    pub(crate) pool_max: u32,
    pub(crate) effective_reserve: u32,
}

impl PressureSnapshot {
    pub(crate) fn in_flight(self) -> u32 {
        self.pool_size.saturating_sub(self.pool_idle)
    }

    pub(crate) fn background_budget(self) -> u32 {
        self.pool_max.saturating_sub(self.effective_reserve)
    }

    pub(crate) fn should_yield(self) -> bool {
        self.effective_reserve != 0 && self.in_flight() >= self.background_budget()
    }
}

pub(crate) fn pressure_snapshot(pool: &PgPool) -> PressureSnapshot {
    PressureSnapshot {
        effective_reserve: super::FOREGROUND_RESERVE.load(Ordering::Acquire),
        pool_size: pool.size(),
        pool_idle: pool.num_idle() as u32,
        pool_max: pool.options().get_max_connections(),
    }
}

// Existing boundary tests use the same snapshot predicate as the runtime gate.
#[cfg(test)]
pub(super) fn should_yield_for_counters(
    reserve: u32,
    size: u32,
    num_idle: u32,
    max_connections: u32,
) -> bool {
    PressureSnapshot {
        effective_reserve: reserve,
        pool_size: size,
        pool_idle: num_idle,
        pool_max: max_connections,
    }
    .should_yield()
}
