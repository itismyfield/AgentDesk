//! The tracked input frame: a token around the rendered prompt, its digest and an exact parser.

use sha2::{Digest, Sha256};

use crate::services::tui_o::shadow::ShadowProvider;

const START: &str = "[adk:tok=";
const END: &str = "[adk:end=";

/// The normalization a provider applies to stored prompt text; Claude stores a tab as four spaces.
pub fn profile(provider: ShadowProvider) -> &'static str {
    match provider {
        ShadowProvider::Claude => "claude-lf-tab4",
        ShadowProvider::Codex => "codex-lf",
    }
}

/// The text a tracked attempt submits; both markers are whole lines.
pub fn render(token: &str, rendered: &str) -> String {
    format!("{START}{token}]\n{rendered}\n{END}{token}]")
}

/// Line endings become LF under every profile; an unknown profile matches nothing.
fn normalize(profile: &str, text: &str) -> Option<String> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    match profile {
        "claude-lf-tab4" => Some(text.replace('\t', "    ")),
        "codex-lf" => Some(text),
        _ => None,
    }
}

/// Lowercase hex sha256 of the normalized frame, as registered before the effect.
pub fn digest(profile: &str, frame: &str) -> Option<String> {
    normalize(profile, frame).map(|text| hex::encode(Sha256::digest(text.as_bytes())))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Framed {
    pub token: String,
    pub digest: String,
}

/// Complete frames in `text`, in order. A quoted or prefixed marker, a start without its own
/// end line before the next marker, or an empty body yields nothing.
pub fn frames(profile: &str, text: &str) -> Vec<Framed> {
    let Some(text) = normalize(profile, text) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let mut found = Vec::new();
    let mut at = 0;
    while at < lines.len() {
        let Some(token) = marker(lines[at], START) else {
            at += 1;
            continue;
        };
        let next = (at + 1..lines.len()).find(|&line| {
            marker(lines[line], START).is_some() || marker(lines[line], END).is_some()
        });
        match next {
            Some(end) if end > at + 1 && marker(lines[end], END) == Some(token) => {
                let frame = lines[at..=end].join("\n");
                found.push(Framed {
                    token: token.to_owned(),
                    digest: hex::encode(Sha256::digest(frame.as_bytes())),
                });
                at = end + 1;
            }
            Some(end) => at = end,
            None => break,
        }
    }
    found
}

fn marker<'a>(line: &'a str, open: &str) -> Option<&'a str> {
    let token = line.strip_prefix(open)?.strip_suffix(']')?;
    let hex = |b: u8| matches!(b, b'0'..=b'9' | b'a'..=b'f');
    (token.len() == 32 && token.bytes().all(hex)).then_some(token)
}
