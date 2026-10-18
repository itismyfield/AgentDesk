//! How many times a rebind replaced each relay output path with a new file, so a
//! reader that sees the path's file change can tell a rebuild from a rotation.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

static REPLACEMENTS: LazyLock<Mutex<HashMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A rebind replaced `path` with a new file whose bytes start a new coordinate
/// space, unlike a rotation, whose new file keeps the delivered tail.
pub(in crate::services::discord) fn note_relay_output_replaced(path: &str) {
    let mut replacements = REPLACEMENTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    *replacements.entry(path.to_string()).or_default() += 1;
}

pub(in crate::services::discord) fn relay_output_replacements(path: &str) -> u64 {
    REPLACEMENTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(path)
        .copied()
        .unwrap_or(0)
}
