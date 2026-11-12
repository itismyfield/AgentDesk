mod types;
pub(crate) use types::{DecisionResult, OffReason};

/// The dormant production entry never captures keys or constructs a transport.
pub(crate) fn availability() -> DecisionResult {
    DecisionResult::Off(OffReason::NotInstalled)
}

#[cfg(test)]
mod policy;
#[cfg(test)]
mod runtime;
#[cfg(test)]
mod tests;
