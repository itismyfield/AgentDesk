mod decision_route;
mod tuning_aggregate;
mod verdict_route;

pub(crate) use crate::services::review_decision::spawn_aggregate_if_needed_with_pg;
pub use decision_route::submit_review_decision;
pub use tuning_aggregate::aggregate_review_tuning;
// #3863: single source of truth for the commit-SHA guard. The recovery route in
// `super::reviews` reuses this exact predicate instead of copy-pasting the regex.
pub(crate) use verdict_route::is_valid_commit_sha;
pub use verdict_route::submit_verdict;
// #3037: review loopback request DTOs (`ReviewDecisionBody`, `SubmitVerdictBody`)
// were relocated to `crate::services::review_decision`; all consumers reference
// them there directly, so no route-layer facade re-export is required.
