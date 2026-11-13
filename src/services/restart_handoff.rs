pub(crate) mod proof;
pub(crate) mod store;

pub(crate) struct RestartMetadata {
    pub(crate) generation: u64,
    pub(crate) doctor_summary: serde_json::Value,
}

pub(crate) fn sample_metadata(read_doctor: fn(bool) -> serde_json::Value) -> RestartMetadata {
    RestartMetadata {
        generation: crate::services::process_generation::process_generation(),
        doctor_summary: read_doctor(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_reads_process_binding_instead_of_foreign_disk_counter() {
        let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let disk = crate::services::process_generation::load_generation();
        let pinned = disk.wrapping_add(1);
        crate::services::process_generation::set_process_generation_for_tests(Some(pinned));
        let sampled = sample_metadata(|_| serde_json::json!({"startup": "ready"}));
        crate::services::process_generation::set_process_generation_for_tests(None);
        assert_eq!(sampled.generation, pinned);
        assert_eq!(
            sampled.doctor_summary,
            serde_json::json!({"startup": "ready"})
        );
    }
}
