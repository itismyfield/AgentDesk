use super::{UnterminatedSource, braced_item_span, rust_code_mask, strip_rust_comments};

fn strip(source: &str) -> String {
    strip_rust_comments(source).expect("terminated source")
}

fn mask(source: &str) -> String {
    rust_code_mask(source).expect("terminated source")
}

#[test]
fn nested_block_comments_are_blank_only_inside_comments() {
    let source = "/* outer /* fake_call(); */ fake_call(); */ real_call();\n\
                  /// fake_call();\n//! fake_call();\n/**/ /*/ fake_call(); */ tail();\n";
    for view in [strip(source), mask(source)] {
        assert!(!view.contains("fake_call"), "comment survived: {view:?}");
        assert!(view.contains("real_call();"), "code blanked: {view:?}");
        assert!(
            view.contains("tail();"),
            "code after `/*/` blanked: {view:?}"
        );
        assert_eq!(view.len(), source.len());
    }
}

#[test]
fn raw_hash_strings_preserve_comment_markers() {
    let source = r###"let text = r##"// keep /* keep */ "# quote"##; // drop_me
let plain = r"// keep_plain"; /* drop_two */ after();
"###;
    let stripped = strip(source);
    assert!(stripped.contains(r###"r##"// keep /* keep */ "# quote"##;"###));
    assert!(stripped.contains(r#"r"// keep_plain";"#));
    assert!(!stripped.contains("drop_me") && !stripped.contains("drop_two"));
    let masked = mask(source);
    assert!(
        !masked.contains("keep"),
        "raw literal not masked: {masked:?}"
    );
    assert!(masked.contains("let text =") && masked.contains("after();"));
}

#[test]
fn ordinary_strings_preserve_slashes_and_escapes() {
    let source = r#"let url = "http://host/* x */ \" // still"; call(); // drop_me
let dir = "dir\\"; /* drop_two */ next();
"#;
    let stripped = strip(source);
    assert!(stripped.contains(r#""http://host/* x */ \" // still""#));
    assert!(stripped.contains(r#""dir\\";"#));
    assert!(stripped.contains("call();") && stripped.contains("next();"));
    assert!(!stripped.contains("drop_me") && !stripped.contains("drop_two"));
    let masked = mask(source);
    assert!(!masked.contains("http") && !masked.contains("dir\\"));
    assert!(masked.contains("call();") && masked.contains("next();"));
}

#[test]
fn slash_char_is_not_a_comment() {
    let source = r#"let s = '/'; let t = '*'; let q = '"'; after_quote(); let k = '\'';
let n = '\n'; let u = '\u{1F600}'; let h = '한'; // drop_me
if c == '/' { x() } /* drop_two */ tail();
"#;
    let stripped = strip(source);
    for kept in [
        "'/'",
        "'*'",
        r#"'"'"#,
        r"'\''",
        r"'\n'",
        r"'\u{1F600}'",
        "'한'",
        "tail();",
    ] {
        assert!(stripped.contains(kept), "{kept} lost: {stripped:?}");
    }
    assert!(!stripped.contains("drop_me") && !stripped.contains("drop_two"));
    let masked = mask(source);
    assert!(!masked.contains("'/'") && !masked.contains('한'));
    assert!(masked.contains("after_quote();") && masked.contains("tail();"));
}

#[test]
fn lifetimes_do_not_open_char_literals() {
    let source = "fn f<'a>(s: &'a str) -> &'a str { 'outer: loop { break 'outer; } s } // drop_me\n\
                  const X: &'static str = \"x\"; /* drop_two */ kept();\n";
    for view in [strip(source), mask(source)] {
        assert!(
            !view.contains("drop_me") && !view.contains("drop_two"),
            "{view:?}"
        );
        assert!(view.contains("kept();"), "{view:?}");
        assert!(view.contains("<'a>(s: &'a str)") && view.contains("'outer: loop"));
    }
}

#[test]
fn comment_blanking_preserves_offsets_and_token_separation() {
    let source = "foo/*주석 é*/bar\r\nline2 // 끝 ✓\r\nlast";
    for view in [strip(source), mask(source)] {
        assert_eq!(view.len(), source.len());
        assert_eq!(
            view.split_whitespace().collect::<Vec<_>>(),
            ["foo", "bar", "line2", "last"]
        );
        assert_eq!(view.matches("\r\n").count(), 2);
        for token in ["bar", "line2", "last"] {
            assert_eq!(view.find(token), source.find(token), "{token} moved");
        }
    }
    let literal = "let a = \"x\ny\"; b();";
    let masked = mask(literal);
    assert_eq!(masked.find("b();"), literal.find("b();"));
    assert_eq!(masked.lines().count(), literal.lines().count());
}

#[test]
fn unterminated_source_is_rejected() {
    for (open, closed) in [
        ("/* open", "/* open */"),
        ("/* a /* b */", "/* a /* b */ */"),
        ("\"open", "\"open\""),
        ("\"esc\\\"", "\"esc\\\"\""),
        ("r#\"open\"", "r#\"open\"#"),
        ("r##\"x\"#", "r##\"x\"##"),
        ("b\"open", "b\"open\""),
        ("br#\"x", "br#\"x\"#"),
        ("c\"x", "c\"x\""),
        ("cr#\"x\"", "cr#\"x\"#"),
        ("let c = '\\", "let c = '\\\\';"),
        ("let c = '\\u{41", "let c = '\\u{41}';"),
        ("let c = b'", "let c = b'x';"),
        ("r##x", "r#x"),
    ] {
        assert!(strip_rust_comments(open).is_err(), "accepted {open:?}");
        assert!(rust_code_mask(open).is_err(), "masked {open:?}");
        assert!(strip_rust_comments(closed).is_ok(), "rejected {closed:?}");
        assert!(rust_code_mask(closed).is_ok(), "rejected mask {closed:?}");
    }
    assert_eq!(
        strip_rust_comments("ok(); /* x"),
        Err(UnterminatedSource {
            kind: "block comment",
            offset: 6
        })
    );
    assert_eq!(
        strip("ok(); // trailing"),
        format!("ok();{}", " ".repeat(12))
    );
}

#[test]
fn byte_and_c_string_prefixes_follow_the_same_transitions() {
    let source = r####"let a = b"// k1"; let b = br##"/* k2 "# */"##; let c = c"// k3";
let d = cr##"x "# // k4"##; let e = b'/'; let f = b'\''; // drop_me
let r#type = 0b10u8; let br = 1; let cr = br; // drop_two
end();
"####;
    let stripped = strip(source);
    for kept in [
        "k1",
        "k2",
        "k3",
        "k4",
        "b'/'",
        r"b'\''",
        "r#type",
        "let cr = br;",
    ] {
        assert!(stripped.contains(kept), "{kept} lost: {stripped:?}");
    }
    assert!(!stripped.contains("drop_me") && !stripped.contains("drop_two"));
    let masked = mask(source);
    for gone in ["k1", "k2", "k3", "k4", "b'/'"] {
        assert!(!masked.contains(gone), "{gone} not masked: {masked:?}");
    }
    assert!(masked.contains("r#type") && masked.contains("let cr = br;"));
    assert!(masked.contains("end();"));
}

#[test]
fn braces_inside_literals_and_comments_do_not_move_item_span() {
    let source = r##"// fn target() { decoy }
const DECOY: &str = "fn target() {";
fn target() -> &'static str {
    let s = "}"; let c = '}'; /* } */ // }
    if true { "{" } else { r#"}"# }
}
fn after() {}
"##;
    let span = braced_item_span(&mask(source), "fn target()").expect("item span");
    let item = &source[span];
    assert!(
        item.starts_with("fn target() -> &'static str {"),
        "{item:?}"
    );
    assert!(item.ends_with("r#\"}\"# }\n}"), "{item:?}");
    assert!(braced_item_span(&mask("fn x() {"), "fn x()").is_none());
    assert!(braced_item_span(&mask("fn x() {}"), "fn y()").is_none());
    assert!(braced_item_span(&mask("fn x(); fn z() {}"), "fn x()").is_none());
}
