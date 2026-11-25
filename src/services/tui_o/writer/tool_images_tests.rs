use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

use super::tool_images::{ImageBody, OmitReason, ToolImage, extract};
use crate::services::discord::outbound::image_format::MAX_IMAGE_BYTES;
use crate::services::tui_o::shadow::ShadowProvider::{Claude, Codex};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
const PNG_SHA256: &str = "4c4b6a3be1314ab86138bef4314dde022e600960d8689a2c8f8631802d20dab6";

fn claude_row(blocks: Value) -> Value {
    json!({"type":"user","uuid":"row","message":{"content":[
        {"type":"tool_result","tool_use_id":"toolu_1","content":blocks}
    ]}})
}

fn claude_png(media_type: &str, data: &[u8]) -> Value {
    let source = json!({"type":"base64","media_type":media_type,"data":STANDARD.encode(data)});
    claude_row(json!([{"type":"text","text":"read"}, {"type":"image","source":source}]))
}

fn codex_row(kind: &str, output: Value) -> Value {
    json!({"type":"response_item","payload":{"type":kind,"call_id":"call_1","output":output}})
}

fn omitted(images: Vec<ToolImage>) -> Vec<OmitReason> {
    let reason = |image: ToolImage| match image.body {
        ImageBody::Omitted(reason) => reason,
        ImageBody::Inline { .. } => panic!("expected an omitted image"),
    };
    images.into_iter().map(reason).collect()
}

#[test]
fn tool_images_claude_png_extracts_one() {
    let images = extract(Claude, &claude_png("image/PNG", PNG));
    let expected = ImageBody::Inline {
        media_type: "image/png".into(),
        sha256: PNG_SHA256.into(),
        bytes: PNG.to_vec(),
    };
    let found: Vec<_> = images
        .iter()
        .map(|i| (i.call_id.as_str(), i.index))
        .collect();
    assert_eq!(found, [("toolu_1", 0)]);
    assert_eq!(images[0].body, expected);
}

#[test]
fn tool_images_codex_data_url_extracts_one() {
    let url = format!("data:image/png;base64,{}", STANDARD.encode(PNG));
    let output = json!([{"type":"input_text","text":"x"}, {"type":"input_image","image_url":url}]);
    for kind in ["function_call_output", "custom_tool_call_output"] {
        let images = extract(Codex, &codex_row(kind, output.clone()));
        assert_eq!(images.len(), 1, "{kind}");
        assert_eq!(images[0].call_id, "call_1");
        let ImageBody::Inline { sha256, .. } = &images[0].body else {
            panic!("{kind} image must be inline");
        };
        assert_eq!(sha256, PNG_SHA256);
    }
}

#[test]
fn tool_images_codex_file_id_is_external_ref() {
    let output = json!([
        {"type":"input_image","file_id":"file-1"},
        {"type":"input_image","image_url":"https://example.invalid/a.png"},
    ]);
    let images = extract(Codex, &codex_row("function_call_output", output));
    let indexes: Vec<usize> = images.iter().map(|image| image.index).collect();
    assert_eq!(indexes, [0, 1]);
    assert_eq!(omitted(images), [OmitReason::ExternalRef; 2]);
}

#[test]
fn tool_images_over_limit_too_large() {
    let mut bytes = vec![0; MAX_IMAGE_BYTES + 1];
    bytes[..PNG.len()].copy_from_slice(PNG);
    let images = extract(Claude, &claude_png("image/png", &bytes));
    assert_eq!(omitted(images), [OmitReason::TooLarge]);
}

#[test]
fn tool_images_bad_magic_unsupported() {
    for (media_type, data) in [
        ("image/png", &b"GIF89a"[..]),
        ("image/bmp", &b"BM"[..]),
        ("image/png", &b""[..]),
    ] {
        let images = extract(Claude, &claude_png(media_type, data));
        assert_eq!(
            omitted(images),
            [OmitReason::UnsupportedType],
            "{media_type}"
        );
    }
}

#[test]
fn tool_images_text_only_yields_nothing() {
    let text = json!({"type":"input_text","text":"done"});
    for (provider, row) in [
        (Claude, claude_row(json!("plain text"))),
        (Claude, claude_row(json!([{"type":"text","text":"done"}]))),
        (
            Codex,
            codex_row("function_call_output", json!("plain text")),
        ),
        (Codex, codex_row("function_call_output", json!([text]))),
    ] {
        assert!(extract(provider, &row).is_empty(), "{row}");
    }
}
