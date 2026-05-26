/// Compare bearer/admin tokens without short-circuiting on the first mismatch.
///
/// The loop consumes both byte slices up to the longer length and folds the
/// length mismatch into the accumulator, so equal-prefix and different-length
/// probes do not exit earlier than same-length probes.
pub(crate) fn constant_time_token_eq(expected: &str, supplied: &str) -> bool {
    let expected = expected.as_bytes();
    let supplied = supplied.as_bytes();
    let max_len = expected.len().max(supplied.len());
    let mut diff = expected.len() ^ supplied.len();

    for index in 0..max_len {
        let expected_byte = expected.get(index).copied().unwrap_or(0);
        let supplied_byte = supplied.get(index).copied().unwrap_or(0);
        diff |= (expected_byte ^ supplied_byte) as usize;
    }

    diff == 0
}

#[cfg(test)]
mod tests {
    use super::constant_time_token_eq;

    #[test]
    fn constant_time_token_eq_accepts_exact_match() {
        assert!(constant_time_token_eq("secret-token", "secret-token"));
    }

    #[test]
    fn constant_time_token_eq_rejects_same_length_mismatch() {
        assert!(!constant_time_token_eq("secret-token", "secret-taken"));
    }

    #[test]
    fn constant_time_token_eq_rejects_prefix_and_length_mismatch() {
        assert!(!constant_time_token_eq("secret-token", "secret"));
        assert!(!constant_time_token_eq("secret", "secret-token"));
    }
}
