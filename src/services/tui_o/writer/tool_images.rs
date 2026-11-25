//! Images inside tool results, read from raw transcript rows; nothing here records or posts.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::services::discord::outbound::image_format::{self, Rejected};
use crate::services::tui_o::shadow::ShadowProvider;

#[cfg(test)]
mod kind_tests;
pub(crate) mod turns;
#[cfg(test)]
mod turns_tests;

/// A settled native turn: its opener id, or `edge:{source}:{start}` when capture began mid-turn.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct NativeTurn(String);

impl NativeTurn {
    pub(crate) fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The native key of an image in `turn`; the same bytes within one turn share it.
pub(crate) fn image_native_key(turn: &NativeTurn, sha256: &str) -> String {
    format!("img:{}:{sha256}", turn.0)
}

/// Why an image is not attached; `NotConfirmed` comes from settlement, never from extraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OmitReason {
    TooLarge,
    UnsupportedType,
    /// A file id or URL whose bytes are not in the transcript.
    ExternalRef,
    NotConfirmed,
}

impl OmitReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::UnsupportedType => "unsupported_type",
            Self::ExternalRef => "external_ref",
            Self::NotConfirmed => "not_confirmed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImageBody {
    /// Decoded bytes that passed the shared MIME, size and magic checks; `sha256` is lowercase hex.
    Inline {
        media_type: String,
        sha256: String,
        bytes: Vec<u8>,
    },
    Omitted(OmitReason),
}

/// One image block of a tool result; `index` counts image blocks within that result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolImage {
    /// Claude's `tool_use_id` or Codex's `call_id`.
    pub(crate) call_id: String,
    pub(crate) index: usize,
    pub(crate) body: ImageBody,
}

/// Every image of the tool results in one raw `record`; a row without one yields nothing.
pub(crate) fn extract(provider: ShadowProvider, record: &Value) -> Vec<ToolImage> {
    let results = match provider {
        ShadowProvider::Claude => claude_results(record),
        ShadowProvider::Codex => codex_result(record).into_iter().collect(),
        ShadowProvider::Antigravity => Vec::new(),
    };
    let images = results.into_iter().flat_map(|(call_id, bodies)| {
        bodies
            .into_iter()
            .enumerate()
            .map(move |(index, body)| ToolImage {
                call_id: call_id.to_owned(),
                index,
                body,
            })
    });
    images.collect()
}

fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `(call id, image bodies)` of one tool result, or `None` when its content holds no image block.
fn images_of<'a>(
    call_id: Option<&'a str>,
    parts: Option<&'a Value>,
    image_type: &str,
    body: fn(&Value) -> ImageBody,
) -> Option<(&'a str, Vec<ImageBody>)> {
    let call_id = call_id.filter(|id| !id.is_empty())?;
    let parts = parts?.as_array()?.iter();
    let bodies: Vec<ImageBody> = parts
        .filter(|part| str_at(part, "type") == Some(image_type))
        .map(body)
        .collect();
    (!bodies.is_empty()).then_some((call_id, bodies))
}

fn claude_results(record: &Value) -> Vec<(&str, Vec<ImageBody>)> {
    if str_at(record, "type") != Some("user") {
        return Vec::new();
    }
    let items = record.pointer("/message/content").and_then(Value::as_array);
    let results = items.into_iter().flatten();
    let results = results.filter(|item| str_at(item, "type") == Some("tool_result"));
    let images = results.filter_map(|item| {
        let call_id = str_at(item, "tool_use_id");
        images_of(call_id, item.get("content"), "image", claude_image)
    });
    images.collect()
}

fn claude_image(part: &Value) -> ImageBody {
    let source = part.get("source").unwrap_or(&Value::Null);
    match str_at(source, "type") {
        Some("base64") => inline(str_at(source, "media_type"), str_at(source, "data")),
        Some("url" | "file") => ImageBody::Omitted(OmitReason::ExternalRef),
        _ => ImageBody::Omitted(OmitReason::UnsupportedType),
    }
}

fn codex_result(record: &Value) -> Option<(&str, Vec<ImageBody>)> {
    let payload = record.get("payload")?;
    let output = matches!(
        (str_at(record, "type"), str_at(payload, "type")),
        (
            Some("response_item"),
            Some("function_call_output" | "custom_tool_call_output")
        )
    );
    let call_id = output.then(|| str_at(payload, "call_id")).flatten();
    images_of(call_id, payload.get("output"), "input_image", codex_image)
}

/// Only an inline `data:<mime>;base64,` URL carries bytes; any other URL or a file id is a reference.
fn codex_image(part: &Value) -> ImageBody {
    let Some(url) = str_at(part, "image_url") else {
        return ImageBody::Omitted(match part.get("file_id") {
            Some(_) => OmitReason::ExternalRef,
            None => OmitReason::UnsupportedType,
        });
    };
    match url
        .strip_prefix("data:")
        .map(|url| url.split_once(";base64,"))
    {
        Some(Some((media_type, data))) => inline(Some(media_type), Some(data)),
        Some(None) => ImageBody::Omitted(OmitReason::UnsupportedType),
        None => ImageBody::Omitted(OmitReason::ExternalRef),
    }
}

fn inline(media_type: Option<&str>, data: Option<&str>) -> ImageBody {
    let omitted = ImageBody::Omitted;
    let (Some(media_type), Some(data)) = (media_type, data) else {
        return omitted(OmitReason::UnsupportedType);
    };
    if !image_format::encoded_fits(data.len()) {
        return omitted(OmitReason::TooLarge);
    }
    let Ok(bytes) = STANDARD.decode(data) else {
        return omitted(OmitReason::UnsupportedType);
    };
    let media_type = media_type.to_ascii_lowercase();
    match image_format::check_bytes(&media_type, &bytes) {
        Ok(()) => ImageBody::Inline {
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            media_type,
            bytes,
        },
        Err(Rejected::Size) if !bytes.is_empty() => omitted(OmitReason::TooLarge),
        Err(_) => omitted(OmitReason::UnsupportedType),
    }
}
