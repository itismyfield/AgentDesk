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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_keep_each_status_and_owned_session() {
        for (outcome, status, text, owned) in [
            (
                HeadlessTurnStartOutcome::accepted("turn-7".into()),
                HeadlessTurnStartStatus::Accepted,
                "accepted",
                None,
            ),
            (
                HeadlessTurnStartOutcome::started("turn-7".into(), Some("owned-8".into())),
                HeadlessTurnStartStatus::Started,
                "started",
                Some("owned-8".to_string()),
            ),
            (
                HeadlessTurnStartOutcome::started("turn-7".into(), None),
                HeadlessTurnStartStatus::Started,
                "started",
                None,
            ),
            (
                HeadlessTurnStartOutcome::consumed("turn-7".into()),
                HeadlessTurnStartStatus::Consumed,
                "consumed",
                None,
            ),
        ] {
            assert_eq!(
                outcome,
                HeadlessTurnStartOutcome {
                    turn_id: "turn-7".into(),
                    status,
                    owned_session_key: owned,
                }
            );
            assert_eq!(outcome.status.as_str(), text);
            assert_eq!(
                outcome.status.is_accepted(),
                status == HeadlessTurnStartStatus::Accepted
            );
        }
    }

    #[test]
    fn errors_keep_display_and_exact_deferred_classification() {
        for error in [
            HeadlessTurnStartError::Conflict("mailbox busy".into()),
            HeadlessTurnStartError::Internal("runtime unavailable".into()),
            HeadlessTurnStartError::InvalidTarget("channel not a thread".into()),
        ] {
            let text = match &error {
                HeadlessTurnStartError::Conflict(text)
                | HeadlessTurnStartError::Internal(text)
                | HeadlessTurnStartError::InvalidTarget(text) => text,
            };
            assert_eq!(error.to_string(), *text);
            assert!(!error.is_original_start_deferred());
            let as_error: &dyn std::error::Error = &error;
            assert!(as_error.source().is_none());
            assert_eq!(error, error.clone());
        }
        let message = "original bridge start deferred; caller retry required";
        let deferred = HeadlessTurnStartError::original_start_deferred();
        assert_eq!(deferred, HeadlessTurnStartError::Internal(message.into()));
        assert_eq!(deferred.to_string(), message);
        assert!(deferred.is_original_start_deferred());
        for error in [
            HeadlessTurnStartError::Conflict(message.into()),
            HeadlessTurnStartError::InvalidTarget(message.into()),
            HeadlessTurnStartError::Internal(format!("{message}!")),
        ] {
            assert!(!error.is_original_start_deferred());
        }
    }
}
