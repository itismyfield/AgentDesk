//! Headless turn start results shared by the Discord router and the routine and schedule runners.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeadlessTurnStartOutcome {
    pub turn_id: String,
    pub status: HeadlessTurnStartStatus,
    pub owned_session_key: Option<String>,
}

impl HeadlessTurnStartOutcome {
    pub(crate) fn accepted(turn_id: String) -> Self {
        Self {
            turn_id,
            status: HeadlessTurnStartStatus::Accepted,
            owned_session_key: None,
        }
    }

    pub(crate) fn started(turn_id: String, owned_session_key: Option<String>) -> Self {
        Self {
            turn_id,
            status: HeadlessTurnStartStatus::Started,
            owned_session_key,
        }
    }

    pub(crate) fn consumed(turn_id: String) -> Self {
        Self {
            turn_id,
            status: HeadlessTurnStartStatus::Consumed,
            owned_session_key: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadlessTurnStartStatus {
    // Owner admission stays dormant until its production ingress is wired.
    Accepted,
    Started,
    Consumed,
}

impl HeadlessTurnStartStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Started => "started",
            Self::Consumed => "consumed",
        }
    }

    pub(crate) fn is_accepted(self) -> bool {
        match self {
            Self::Accepted => true,
            Self::Started | Self::Consumed => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Every error from the reserved headless-start API is a pre-spawn outcome:
/// neither the provider process nor its turn bridge has been spawned. Once the
/// API returns `Started`, callers must treat any later persistence failure as
/// ambiguous/fail-closed rather than retrying the external turn.
pub(crate) enum HeadlessTurnStartError {
    Conflict(String),
    Internal(String),
    InvalidTarget(String),
}

impl std::fmt::Display for HeadlessTurnStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(message) | Self::Internal(message) | Self::InvalidTarget(message) => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for HeadlessTurnStartError {}

const ORIGINAL_START_DEFERRED: &str = "original bridge start deferred; caller retry required";

impl HeadlessTurnStartError {
    /// A watcher recovery held the channel past the start wait; nothing started, so retry later.
    pub(crate) fn original_start_deferred() -> Self {
        Self::Internal(ORIGINAL_START_DEFERRED.into())
    }

    pub(crate) fn is_original_start_deferred(&self) -> bool {
        matches!(self, Self::Internal(message) if message == ORIGINAL_START_DEFERRED)
    }
}
