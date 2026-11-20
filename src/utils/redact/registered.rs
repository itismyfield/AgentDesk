use std::ops::Range;

use super::KNOWN_SECRETS;

pub(crate) fn contains_registered_secret(input: &str) -> bool {
    let secrets = KNOWN_SECRETS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    secrets.iter().any(|secret| input.contains(secret))
}

pub(crate) fn registered_secret_ranges(input: &str) -> Vec<Range<usize>> {
    let secrets = KNOWN_SECRETS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    secrets
        .iter()
        .flat_map(|secret| {
            input
                .match_indices(secret)
                .map(|(start, matched)| start..start + matched.len())
        })
        .collect()
}
