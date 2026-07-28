/// Canonical compatibility contract for the externally named `thread_group`
/// field. Prompt and API documentation must reuse this sentence verbatim so
/// callers cannot receive the runtime semantics in reverse.
pub(crate) const THREAD_GROUP_SERIAL_LANE_CONTRACT: &str = "Within one `batch_phase`, entries with the same `thread_group` share a serial lane with at most one active entry; entries with different `thread_group` values may run in parallel up to available capacity, and dependency-related entries must share a serial lane or use separate `batch_phase` values.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_contract_states_runtime_direction_and_dependency_rule() {
        assert_eq!(
            THREAD_GROUP_SERIAL_LANE_CONTRACT,
            "Within one `batch_phase`, entries with the same `thread_group` share a serial lane with at most one active entry; entries with different `thread_group` values may run in parallel up to available capacity, and dependency-related entries must share a serial lane or use separate `batch_phase` values."
        );
    }
}
