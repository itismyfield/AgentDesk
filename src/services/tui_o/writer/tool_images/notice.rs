//! Texts an image obligation posts: an attachment's caption and a result's omission notice.
//! Both carry a marker so history settlement can single out the post.

use sha2::{Digest, Sha256};

use crate::services::tui_o::shadow::UnitKey;
use crate::services::tui_o::store::ledger::tool_images::ImageState;

/// Ten hex digits of the key's SHA-256: distinct per turn and content, stable across restarts.
pub(crate) fn marker(native_key: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(native_key.as_bytes()));
    digest[..10].to_owned()
}

/// `image-{n}.{ext}` for the n-th image of its result.
pub(crate) fn display_name(index: usize, media_type: &str) -> String {
    let ext = match media_type {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    };
    format!("image-{}.{ext}", index + 1)
}

pub(crate) fn caption(display_name: &str, marker: &str) -> String {
    format!("-# 이미지 {display_name} · {marker}")
}

/// One line for a result's omitted images, rebuilt from the ledger so every attempt matches.
/// It never claims an image did not arrive: `not_confirmed` reads as an unconfirmed delivery.
pub(crate) fn payload(images: &ImageState, notice: &UnitKey) -> String {
    let reasons = images.notice_members(notice);
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for member in &reasons {
        *counts.entry(label(&member.reason)).or_default() += 1;
    }
    let parts: Vec<String> = counts
        .iter()
        .map(|(label, count)| format!("{label} {count}"))
        .collect();
    let marker = marker(&notice.native_key);
    format!(
        "-# 이미지 {}장 생략({}) · {marker}",
        reasons.len(),
        parts.join(", ")
    )
}

fn label(reason: &str) -> &'static str {
    match reason {
        "too_large" => "크기 초과",
        "unsupported_type" => "형식 미지원",
        "external_ref" => "외부 참조",
        "not_confirmed" => "전달 확인 실패",
        _ => "기타",
    }
}
