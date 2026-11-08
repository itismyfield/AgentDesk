//! Comment-free views of `include_str!` inputs for the Discord intake source guards.
//! Lexical tripwires: no alias, macro expansion, reachability or await pairing.

use std::ops::Range;

use crate::test_support::{braced_item_span, rust_code_mask, rust_spans, strip_rust_comments};

/// One source with comments blanked (`code`), literals blanked too (`mask`) and literal
/// token ranges (`literals`), all in the source's byte offsets.
#[derive(Clone, Debug)]
pub(in crate::services::discord) struct Lexed {
    code: String,
    mask: String,
    literals: Vec<Range<usize>>,
}

impl Lexed {
    pub(in crate::services::discord) fn new(source: &str) -> Self {
        Self {
            code: strip_rust_comments(source).expect("guarded source lexes"),
            mask: rust_code_mask(source).expect("guarded source lexes"),
            literals: rust_spans(source).expect("guarded source lexes").literals,
        }
    }

    /// Comment-free text; literals stay, so a banned name inside a string still counts.
    pub(in crate::services::discord) fn text(&self) -> &str {
        &self.code
    }

    /// Offsets where `needle` appears as written: code as code, and each needle literal
    /// as one whole literal token. A needle with no code byte is refused.
    pub(in crate::services::discord) fn find_all(&self, needle: &str) -> Vec<usize> {
        let shape = rust_code_mask(needle).expect("needle lexes on its own");
        assert!(
            !shape.trim().is_empty(),
            "needle `{needle}` has no code byte; anchor it to its call"
        );
        let quoted = rust_spans(needle)
            .expect("needle lexes on its own")
            .literals;
        let whole_token = |range: Range<usize>| {
            self.literals
                .binary_search_by_key(&range.start, |literal| literal.start)
                .is_ok_and(|index| self.literals[index].end == range.end)
        };
        self.code
            .match_indices(needle)
            .map(|(at, _)| at)
            .filter(|at| &self.mask.as_bytes()[*at..*at + needle.len()] == shape.as_bytes())
            .filter(|at| {
                quoted
                    .iter()
                    .all(|literal| whole_token(at + literal.start..at + literal.end))
            })
            .collect()
    }

    pub(in crate::services::discord) fn find(&self, needle: &str) -> Option<usize> {
        self.find_all(needle).first().copied()
    }

    pub(in crate::services::discord) fn contains(&self, needle: &str) -> bool {
        !self.find_all(needle).is_empty()
    }

    pub(in crate::services::discord) fn slice(&self, range: Range<usize>) -> Self {
        let literals = self
            .literals
            .iter()
            .filter(|literal| literal.start < range.end && literal.end > range.start)
            .map(|literal| {
                literal.start.max(range.start) - range.start
                    ..literal.end.min(range.end) - range.start
            })
            .collect();
        Self {
            code: self.code[range.clone()].to_string(),
            mask: self.mask[range].to_string(),
            literals,
        }
    }

    /// From the first as-written `head` through the `}` closing the block it opens;
    /// `None` when `head` is absent, reaches `;` first, or never closes.
    pub(in crate::services::discord) fn block(&self, head: &str) -> Option<Self> {
        let at = self.find(head)?;
        let span = braced_item_span(&self.mask[at..], &self.mask[at..at + head.len()])?;
        Some(self.slice(at + span.start..at + span.end))
    }
}
