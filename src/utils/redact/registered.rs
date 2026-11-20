use std::ops::Range;

use super::KNOWN_SECRETS;

pub(crate) fn with_registered_secret_ranges<T>(
    input: &str,
    use_ranges: impl FnOnce(&mut dyn Iterator<Item = Range<usize>>) -> T,
) -> T {
    let secrets = KNOWN_SECRETS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    use_ranges(&mut secrets.iter().flat_map(|secret| {
        input
            .match_indices(secret)
            .map(|(start, matched)| start..start + matched.len())
    }))
}
