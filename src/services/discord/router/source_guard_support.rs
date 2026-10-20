//! Comment-free views of `include_str!` inputs for the Discord intake source guards.
//! Lexical tripwires: no alias, macro expansion, reachability or await pairing.

use std::ops::Range;

use crate::test_support::{CommentForgery, braced_item_span, rust_code_mask, strip_rust_comments};

/// One source with comments blanked (`code`) and literals blanked too (`mask`); both
/// keep the source's byte offsets, so a position found in one indexes the other.
#[derive(Clone, Debug)]
pub(in crate::services::discord) struct Lexed {
    code: String,
    mask: String,
}

impl Lexed {
    pub(in crate::services::discord) fn new(source: &str) -> Self {
        Self {
            code: strip_rust_comments(source).expect("guarded source lexes"),
            mask: rust_code_mask(source).expect("guarded source lexes"),
        }
    }

    /// Comment-free text; literals stay, so a banned name inside a string still counts.
    pub(in crate::services::discord) fn text(&self) -> &str {
        &self.code
    }

    /// Offsets where `needle` appears as written (code as code, literals as literals); a
    /// needle with no code byte would also match inside a raw string, so it is refused.
    pub(in crate::services::discord) fn find_all(&self, needle: &str) -> Vec<usize> {
        let shape = rust_code_mask(needle).expect("needle lexes on its own");
        assert!(
            !shape.trim().is_empty(),
            "needle `{needle}` has no code byte; anchor it to its call"
        );
        self.code
            .match_indices(needle)
            .map(|(at, _)| at)
            .filter(|at| &self.mask.as_bytes()[*at..*at + needle.len()] == shape.as_bytes())
            .collect()
    }

    pub(in crate::services::discord) fn find(&self, needle: &str) -> Option<usize> {
        self.find_all(needle).first().copied()
    }

    pub(in crate::services::discord) fn find_after(
        &self,
        from: usize,
        needle: &str,
    ) -> Option<usize> {
        self.find_all(needle).into_iter().find(|at| *at >= from)
    }

    pub(in crate::services::discord) fn count(&self, needle: &str) -> usize {
        self.find_all(needle).len()
    }

    pub(in crate::services::discord) fn contains(&self, needle: &str) -> bool {
        !self.find_all(needle).is_empty()
    }

    pub(in crate::services::discord) fn slice(&self, range: Range<usize>) -> Self {
        Self {
            code: self.code[range.clone()].to_string(),
            mask: self.mask[range].to_string(),
        }
    }

    /// Both views without ASCII whitespace (blanked comments included), for checks that
    /// ignore formatting; needles must drop their whitespace the same way.
    pub(in crate::services::discord) fn compact(&self) -> Self {
        let (mut code, mut mask) = (Vec::new(), Vec::new());
        for (byte, masked) in self.code.bytes().zip(self.mask.bytes()) {
            if !byte.is_ascii_whitespace() {
                code.push(byte);
                mask.push(masked);
            }
        }
        let utf8 = |bytes| String::from_utf8(bytes).expect("dropping ASCII keeps UTF-8");
        Self {
            code: utf8(code),
            mask: utf8(mask),
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

/// Rewrites every as-written `needle` as a comment that still holds it verbatim, so a
/// raw `contains` keeps matching while the comment-free guard must not.
pub(in crate::services::discord) fn forge(
    source: &str,
    needle: &str,
    forgery: CommentForgery,
) -> String {
    let starts = Lexed::new(source).find_all(needle);
    assert!(
        !starts.is_empty(),
        "no code occurrence of `{needle}` to forge"
    );
    let hidden = match forgery {
        CommentForgery::Line => format!("// {needle}\n"),
        CommentForgery::NestedBlock => format!("/* outer /* {needle} */ */"),
    };
    let mut forged = source.to_string();
    for at in starts.into_iter().rev() {
        forged.replace_range(at..at + needle.len(), &hidden);
    }
    assert!(forged.contains(needle) && !Lexed::new(&forged).contains(needle));
    forged
}

/// Swaps the first as-written `first` with the first as-written `second` after it.
pub(in crate::services::discord) fn swap(source: &str, first: &str, second: &str) -> String {
    let lexed = Lexed::new(source);
    let a = lexed.find(first).expect("first swap operand");
    let b = lexed
        .find_after(a + first.len(), second)
        .expect("second swap operand follows the first");
    format!(
        "{}{second}{}{first}{}",
        &source[..a],
        &source[a + first.len()..b],
        &source[b + second.len()..]
    )
}

#[test]
fn as_written_matching_skips_prose_and_quoted_copies() {
    let source = "// call(\n/* call( */ let s = \"call(\";\n\
                  fn f() { call(\"x\"); if s.ends_with('{') { call(\"{\") } }\n";
    let lexed = Lexed::new(source);
    assert_eq!(lexed.text().matches("call(").count(), 3);
    assert_eq!((lexed.count("call("), lexed.count("call(\"x\")")), (2, 1));
    let body = lexed
        .block("fn f()")
        .expect("braces inside literals do not end the item");
    assert!(body.text().ends_with("{ call(\"{\") } }"));
    let forged = forge(source, "call(\"x\")", CommentForgery::NestedBlock);
    assert_eq!(Lexed::new(&forged).count("call("), 1);
}

#[test]
fn literal_only_needles_fail_closed() {
    let quoted = Lexed::new("let x = r#\"\"Use `!clear` instead.\"\"#;\n");
    let counted = std::panic::catch_unwind(|| quoted.count("\"Use `!clear` instead.\""));
    assert!(
        counted.is_err(),
        "a literal-only needle counted {counted:?}"
    );
    assert_eq!(
        quoted.count("msg.reply(&ctx.http, \"Use `!clear` instead.\")"),
        0
    );
}
