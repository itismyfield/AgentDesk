//! DL `turn_presence` 시험 가운데 ROOT 항목을 쓰는 것을 ROOT binary에서 돌리는 host. `#[path]` 선언만 둔다.

#[cfg(unix)]
#[path = "turn_presence/activity/unbound/dormancy_tests.rs"]
mod dormancy_tests;

#[cfg(unix)]
#[path = "turn_presence/activity/unbound/live_owner_tests.rs"]
mod owner_tests;

#[cfg(unix)]
#[path = "turn_presence/activity/unbound/stale_binding_tests.rs"]
mod stale_binding_tests;

#[cfg(unix)]
#[path = "turn_presence/activity/unbound/legacy_divergence_tests.rs"]
mod legacy_divergence_tests;

#[cfg(unix)]
#[path = "turn_presence/activity/unbound/superseded_measurement_tests.rs"]
mod superseded_measurement_tests;

#[cfg(unix)]
#[path = "turn_presence/activity/unbound/cross_runtime_owners_tests.rs"]
mod cross_runtime_owners_tests;

#[path = "turn_presence/supervisor_tests.rs"]
mod supervisor_tests;

#[cfg(unix)]
#[path = "turn_presence/activity/unbound/live_review_tests.rs"]
mod live_review_tests;

#[cfg(unix)]
#[path = "turn_presence/activity/unbound/inflight_bound_tests.rs"]
mod inflight_bound_tests;
