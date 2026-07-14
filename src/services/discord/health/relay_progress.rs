use super::stall_liveness::{
    STALL_WATCHDOG_TOOL_PHASE_FRESHNESS_SECS, StallWatchdogLivenessEvidence,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RelayProgressClass {
    ProducingAndDelivering,
    Backpressured,
    Producing,
    Delivering,
    ObservingBacklog,
    NoObservedProgress,
}

impl RelayProgressClass {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::ProducingAndDelivering => "producing_and_delivering",
            Self::Backpressured => "backpressured",
            Self::Producing => "producing",
            Self::Delivering => "delivering",
            Self::ObservingBacklog => "observing_backlog",
            Self::NoObservedProgress => "no_observed_progress",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RelayProgressAssessment {
    pub(super) class: RelayProgressClass,
    pub(super) source_recent: bool,
    pub(super) delivery_recent: bool,
}

fn recent(age_secs: Option<u64>, freshness_secs: u64) -> bool {
    age_secs.is_some_and(|age| age <= freshness_secs)
}

impl StallWatchdogLivenessEvidence {
    pub(super) fn composite_progress(&self, freshness_secs: u64) -> RelayProgressAssessment {
        let source_recent = recent(self.pane_offset_advanced_age_secs, freshness_secs)
            || recent(self.transcript_mtime_age_secs, freshness_secs)
            || recent(self.runtime_activity_age_secs, freshness_secs)
            || recent(self.background_synthetic_activity_age_secs, freshness_secs)
            || recent(
                self.open_tool_execution_age_secs,
                STALL_WATCHDOG_TOOL_PHASE_FRESHNESS_SECS,
            );
        let delivery_recent = recent(self.relay_offset_advanced_age_secs, freshness_secs)
            || recent(self.outbound_activity_age_secs, freshness_secs);
        let class = match (source_recent, delivery_recent) {
            (true, true) => RelayProgressClass::ProducingAndDelivering,
            (true, false) if self.delivery_backlogged => RelayProgressClass::Backpressured,
            (true, false) => RelayProgressClass::Producing,
            (false, true) => RelayProgressClass::Delivering,
            (false, false) if self.has_undelivered_backlog => RelayProgressClass::ObservingBacklog,
            (false, false) => RelayProgressClass::NoObservedProgress,
        };
        RelayProgressAssessment {
            class,
            source_recent,
            delivery_recent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_progress_distinguishes_backpressure_and_delivery_only() {
        let backpressured = StallWatchdogLivenessEvidence {
            pane_offset_advanced_age_secs: Some(1),
            delivery_backlogged: true,
            ..Default::default()
        }
        .composite_progress(120);
        assert_eq!(backpressured.class, RelayProgressClass::Backpressured);

        let delivering = StallWatchdogLivenessEvidence {
            relay_offset_advanced_age_secs: Some(1),
            ..Default::default()
        }
        .composite_progress(120);
        assert_eq!(delivering.class, RelayProgressClass::Delivering);
    }

    #[test]
    fn backlog_grace_is_observation_not_producer_or_delivery_progress() {
        let assessment = StallWatchdogLivenessEvidence {
            delivery_backlogged: true,
            has_undelivered_backlog: true,
            ..Default::default()
        }
        .composite_progress(120);
        assert_eq!(assessment.class, RelayProgressClass::ObservingBacklog);
        assert!(!assessment.source_recent);
        assert!(!assessment.delivery_recent);

        let expired = StallWatchdogLivenessEvidence {
            delivery_backlogged: true,
            ..Default::default()
        }
        .composite_progress(120);
        assert_eq!(expired.class, RelayProgressClass::NoObservedProgress);
    }
}
