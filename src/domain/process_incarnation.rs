/// Admission and restart recovery compare the same process owner incarnation.
static INCARNATION: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| uuid::Uuid::new_v4().to_string());

/// Returns the random UUID minted once, on first use, for this process.
pub(crate) fn incarnation() -> &'static str {
    &INCARNATION
}

#[cfg(test)]
mod tests {
    use super::incarnation;
    use crate::test_support::wait::{WAIT_CAP, join_within};

    #[test]
    fn every_caller_in_one_process_reads_the_same_uuid() {
        let first = incarnation();
        assert!(uuid::Uuid::parse_str(first).is_ok(), "{first}");
        assert_eq!(incarnation(), first);
        let other_thread = join_within(std::thread::spawn(|| incarnation().to_owned()), WAIT_CAP);
        assert_eq!(other_thread, first);
    }
}
