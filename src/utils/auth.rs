/// Compare bearer/admin tokens without short-circuiting on the first mismatch.
///
/// The loop always consumes exactly the expected token length. A caller-controlled
/// token therefore cannot increase comparison work, while equal-prefix and
/// same-length probes still avoid an early byte-mismatch exit.
pub(crate) fn constant_time_token_eq(expected: &str, supplied: &str) -> bool {
    let expected = expected.as_bytes();
    let supplied = supplied.as_bytes();
    let mut diff = 0;

    for index in 0..expected.len() {
        let expected_byte = expected[index];
        let supplied_byte = supplied.get(index).copied().unwrap_or(0);
        diff |= expected_byte ^ supplied_byte;
    }

    diff == 0 && expected.len() == supplied.len()
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
