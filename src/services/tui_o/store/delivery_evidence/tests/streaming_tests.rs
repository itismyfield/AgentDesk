use super::super::streaming;
use super::*;
use std::{cell::Cell, io::Cursor};

fn row(payload: &[u8]) -> Vec<u8> {
    let mut bytes = br#"{"at":"2026-10-29T00:00:00Z","entry":{"type":"input_notice","entry":{"kind":"materialized","evidence":"#.to_vec();
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(b"}}}\n");
    bytes
}

fn accepted(bytes: &[u8]) -> bool {
    streaming::notice(
        Cursor::new(bytes),
        Instant::now() + std::time::Duration::from_secs(10),
    )
    .is_some()
}

#[test]
fn streamed_notice_validates_utf8_and_surrogate_pairs_across_prefix_boundary() {
    let mut payload = vec![b'x'; 1024 * 1024];
    payload.insert(0, b'"');
    payload.extend_from_slice("한글😀".as_bytes());
    payload.extend_from_slice(br#"\uD83D\uDE00"#);
    payload.push(b'"');
    let valid = row(&payload);
    assert!(accepted(&valid));
    let unicode_start = valid
        .windows("한글😀".len())
        .position(|window| window == "한글😀".as_bytes())
        .unwrap();
    for split in unicode_start..unicode_start + "한글😀".len() + 12 {
        let chained = Cursor::new(&valid[..split]).chain(Cursor::new(&valid[split..]));
        assert!(
            streaming::notice(chained, Instant::now() + std::time::Duration::from_secs(10))
                .is_some()
        );
    }
    for bad in [
        vec![0xff],
        vec![0xc0, 0x80],
        vec![0xed, 0xa0, 0x80],
        vec![0xf4, 0x90, 0x80, 0x80],
    ] {
        let mut invalid = payload.clone();
        invalid.splice(invalid.len() - 1..invalid.len() - 1, bad);
        assert!(!accepted(&row(&invalid)));
    }
    for bad in [
        br#"\uD800"#.as_slice(),
        br#"\uDC00"#,
        br#"\uD800\u0041"#,
        br#"\uD800x"#,
    ] {
        let mut invalid = payload.clone();
        invalid.splice(invalid.len() - 1..invalid.len() - 1, bad.iter().copied());
        assert!(!accepted(&row(&invalid)));
    }
}

#[test]
fn streamed_notice_depth_is_bounded_in_ignored_payload() {
    let payload = |depth| format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
    assert!(accepted(&row(payload(125).as_bytes())));
    assert!(!accepted(&row(payload(126).as_bytes())));
}

#[test]
fn streamed_notice_requires_complete_known_headers_and_valid_json() {
    let valid = String::from_utf8(row(br#"{"text":"safe"}"#)).unwrap();
    assert!(accepted(valid.as_bytes()));
    let bad = [
        valid.replace("\"input_notice\"", "\"prepared\""),
        valid.replace("\"materialized\"", "\"future_notice\""),
        valid.replace("\"at\":\"2026-10-29T00:00:00Z\",", ""),
        valid.replace("2026-10-29T00:00:00Z", "invalid-time"),
        valid.replace("\"type\":\"input_notice\",", ""),
        valid.replace("\"kind\":\"materialized\",", ""),
        valid.replace(
            "\"type\":\"input_notice\"",
            "\"type\":\"input_notice\",\"type\":\"input_notice\"",
        ),
        valid.replace(
            "\"kind\":\"materialized\"",
            "\"kind\":\"materialized\",\"kind\":\"materialized\"",
        ),
        valid.replace("\"at\":", "\"at\":\"2026-10-29T00:00:00Z\",\"at\":"),
        valid.replace("safe", "bad\ntext"),
        valid.trim_end().to_owned(),
        format!("{}false\n", valid.trim_end()),
        format!("{}\"unfinished\n", valid.trim_end()),
    ];
    for bytes in bad {
        assert!(
            !accepted(bytes.as_bytes()),
            "unexpected accepted row: {bytes}"
        );
    }
}

#[test]
fn streamed_notice_metadata_allocations_are_bounded() {
    let valid = String::from_utf8(row(br#"{"text":"safe"}"#)).unwrap();
    assert!(accepted(valid.as_bytes()));
    let extension = |key: &str| valid.replacen('{', &format!("{{\"{key}\":0,"), 1);
    assert!(accepted(extension("short_extension").as_bytes()));
    for invalid in [
        extension(&"k".repeat(129)),
        valid.replace("\"at\"", &format!("\"{}\"", "k".repeat(129))),
        valid.replace("2026-10-29T00:00:00Z", &"t".repeat(129)),
        valid.replace("input_notice", &"t".repeat(129)),
        valid.replace("materialized", &"t".repeat(129)),
    ] {
        assert!(!accepted(invalid.as_bytes()));
    }
}

#[test]
fn streamed_notice_rechecks_deadline_inside_a_large_row() {
    let bytes = row(format!("\"{}\"", "x".repeat(8192)).as_bytes());
    assert!(accepted(&bytes));
    streaming::EXPIRE_AFTER_CHUNK.with(|expire| expire.set(true));
    assert!(!accepted(&bytes));
    assert!(
        !streaming::EXPIRE_AFTER_CHUNK.with(Cell::get),
        "the fixture must reach a chunk boundary"
    );
    assert!(accepted(&bytes));
}
