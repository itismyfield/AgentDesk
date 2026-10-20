//! Comment-free views of `include_str!` inputs for the Discord intake source guards.
//! Lexical tripwires: no alias, macro expansion, reachability or await pairing.

use std::ops::Range;

use crate::test_support::{
    CommentForgery, braced_item_span, rust_code_mask, rust_spans, strip_rust_comments,
};

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

    /// Both views without ASCII whitespace outside literals (blanked comments included),
    /// for checks that ignore formatting; literal bytes stay exactly as written.
    pub(in crate::services::discord) fn compact(&self) -> Self {
        let mut quoted = vec![false; self.code.len()];
        for literal in &self.literals {
            quoted[literal.clone()].fill(true);
        }
        let (mut code, mut mask, mut kept) = (Vec::new(), Vec::new(), Vec::new());
        for (at, (byte, masked)) in self.code.bytes().zip(self.mask.bytes()).enumerate() {
            kept.push(code.len());
            if quoted[at] || !byte.is_ascii_whitespace() {
                code.push(byte);
                mask.push(masked);
            }
        }
        kept.push(code.len());
        let utf8 = |bytes| String::from_utf8(bytes).expect("dropping ASCII keeps UTF-8");
        Self {
            literals: self
                .literals
                .iter()
                .map(|literal| kept[literal.start]..kept[literal.end])
                .collect(),
            code: utf8(code),
            mask: utf8(mask),
        }
    }

    /// The (optionally labeled) block after the `=>` of the first arm whose pattern starts
    /// with as-written `head`; `None` for an expression arm or once the enclosing block closes.
    pub(in crate::services::discord) fn arm(&self, head: &str) -> Option<Self> {
        let at = self.find(head)?;
        let mask = self.mask.as_bytes();
        let mut depth = 0usize;
        for index in at..mask.len() {
            match mask[index] {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth = depth.checked_sub(1)?,
                b'=' if depth == 0 && mask.get(index + 1) == Some(&b'>') => {
                    let skip = |from: usize| {
                        from + self.mask[from..].len() - self.mask[from..].trim_start().len()
                    };
                    let mut body = skip(index + 2);
                    if mask.get(body) == Some(&b'\'') {
                        body = skip(body + self.mask[body..].find(':')? + 1);
                    }
                    if mask.get(body) != Some(&b'{') {
                        return None;
                    }
                    let span = braced_item_span(&self.mask[body..], "{")?;
                    return Some(self.slice(body + span.start..body + span.end));
                }
                _ => {}
            }
        }
        None
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

#[test]
fn literal_needle_parts_match_whole_literal_tokens_only() {
    let source = "json!({ \"prefix\\\"epoch_route\": 1 }); json!({ \"epoch_route\": 2 });\n\
                  f(\"a\\\"x\", 1); f(\"x\", 2);\n";
    let lexed = Lexed::new(source);
    assert_eq!(lexed.text().matches("\"epoch_route\":").count(), 2);
    assert_eq!(lexed.count("\"epoch_route\":"), 1);
    assert_eq!(lexed.text().matches("\"x\",").count(), 2);
    assert_eq!(lexed.count("\"x\","), 1);
    assert_eq!(
        lexed
            .slice(source.find("f(").unwrap()..source.len())
            .count("\"x\","),
        1
    );
}

#[test]
fn compact_keeps_literal_bytes_and_drops_only_outside_whitespace() {
    let compact = |source: &str| Lexed::new(source).compact().text().to_string();
    let written = "observe(\n    \"clear_inflight_state_for_reconcile\", // site\n    1,\n)";
    assert_eq!(
        compact(written),
        "observe(\"clear_inflight_state_for_reconcile\",1,)"
    );
    assert_eq!(
        compact(written),
        compact("observe( \"clear_inflight_state_for_reconcile\" , /* x */ 1 , )")
    );
    for spaced in ["for_ reconcile", "for_\treconcile"] {
        let changed = written.replace("for_reconcile", spaced);
        assert_ne!(compact(&changed), compact(written), "{spaced:?}");
    }
    let compacted = Lexed::new(written).compact();
    assert_eq!(
        compacted.count("\"clear_inflight_state_for_reconcile\","),
        1
    );
}

#[test]
fn arm_bodies_skip_pattern_braces_and_refuse_expression_arms() {
    let lexed = Lexed::new(
        "match m { A { x } => { call(x) } B(y) => y, C => { other(\"}\") } D => 'l: { d() } }",
    );
    let body = |head: &str| lexed.arm(head).map(|arm| arm.text().to_string());
    assert_eq!(body("A {").as_deref(), Some("{ call(x) }"));
    assert_eq!(body("B("), None);
    assert_eq!(body("C =>").as_deref(), Some("{ other(\"}\") }"));
    assert_eq!(body("D =>").as_deref(), Some("{ d() }"));
    assert!(Lexed::new("{ f() } g => { h() }").arm("f(").is_none());
}
