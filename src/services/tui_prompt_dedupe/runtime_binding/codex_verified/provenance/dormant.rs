//! Where owed work came from: one complete record inside the span of its exact episode.

use super::CodexEpisodeSpan;
use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwedOrigin {
    pub span: CodexEpisodeSpan,
    pub range: Range<u64>,
}

#[path = "dormant_construction.rs"]
pub(crate) mod construction;
