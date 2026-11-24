//! Test-only reads of the private worker handoff env names for tests hosted outside this module.

pub(crate) const fn failure_marker_worker_env_for_test() -> &'static str {
    super::FAILURE_MARKER_WORKER_ENV
}

pub(crate) const fn non_wait_relay_worker_env_for_test() -> &'static str {
    super::NON_WAIT_RELAY_WORKER_ENV
}
