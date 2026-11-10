//! Test/fuzz entry for the private literal chunk splitter.

pub(crate) fn split_for_fuzz(input: &str, max_chars: usize) -> Vec<String> {
    super::split_literal_chunks(input, max_chars)
}
