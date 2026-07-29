//! Provider-aware guard for assistant prose crossing an AgentDesk boundary.
//!
//! Claude harness control data is never redacted in place. The whole response
//! or streaming frame is classified so callers can pass it unchanged, hold it
//! without advancing delivery state, or replace it with a static safe body.

use pulldown_cmark::{Event, Options, Parser, Tag};

use crate::services::provider::ProviderKind;

pub(crate) const BLOCKED_PROVIDER_OUTPUT_BODY: &str =
    "⚠️ 내부 제어 데이터가 섞인 응답을 차단했습니다. 원문은 전송하지 않았습니다.";
pub(crate) const HELD_PROVIDER_OUTPUT_BODY: &str =
    "⚠️ 응답에 내부 제어 데이터 가능성이 있어 확인될 때까지 전송을 보류했습니다.";

const SYSTEM_BANNER: &str = "[SYSTEM NOTIFICATION - NOT USER INPUT]";
const PRIVATE_TASK_ANCHORS: &[&str] = &[
    "<task-notification>",
    "<task-id>",
    "<tool-use-id>",
    "<output-file>",
];
const CONTROL_ANCHORS: &[&str] = &[
    SYSTEM_BANNER,
    "<task-notification>",
    "</task-notification>",
    "<task-id>",
    "</task-id>",
    "<tool-use-id>",
    "</tool-use-id>",
    "<output-file>",
    "</output-file>",
    "<invoke name=",
    "<parameter",
    "</parameter>",
    "</invoke>",
];

pub(crate) fn contains_provider_control_anchor(text: &str) -> bool {
    let canonical = canonical_control_form(text);
    CONTROL_ANCHORS
        .iter()
        .any(|anchor| canonical.contains(&anchor.to_ascii_lowercase()))
}

pub(crate) fn markdown_contains_provider_control_anchor(text: &str) -> bool {
    contains_provider_control_anchor(&markdown_prose(text))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderOutputKind {
    ClaudeSystemNotification,
    ClaudeToolWrapper,
    PartialControlMarker,
}

impl ProviderOutputKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeSystemNotification => "claude_system_notification",
            Self::ClaudeToolWrapper => "claude_tool_wrapper",
            Self::PartialControlMarker => "partial_control_marker",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderOutputVerdict {
    Clean,
    Hold { kind: ProviderOutputKind },
    Blocked { kind: ProviderOutputKind },
}

pub(crate) fn inspect_provider_output(
    provider: &ProviderKind,
    text: &str,
) -> ProviderOutputVerdict {
    if !matches!(provider, ProviderKind::Claude) {
        return ProviderOutputVerdict::Clean;
    }
    inspect_claude_prose(&markdown_prose(text))
}

/// Streaming is stricter than terminal formatting: a complete standalone
/// control token is not enough to block a legitimate explanation, but it is
/// held while the response is still growing because a later chunk could turn
/// it into one of the forbidden compounds.
pub(crate) fn inspect_provider_streaming_output(
    provider: &ProviderKind,
    text: &str,
) -> ProviderOutputVerdict {
    if !matches!(provider, ProviderKind::Claude) {
        return ProviderOutputVerdict::Clean;
    }
    let prose = markdown_prose(text);
    match inspect_claude_prose(&prose) {
        ProviderOutputVerdict::Clean if contains_control_anchor(&prose) => {
            ProviderOutputVerdict::Hold {
                kind: ProviderOutputKind::PartialControlMarker,
            }
        }
        verdict => verdict,
    }
}

pub(crate) fn inspect_provider_streaming_rollover(
    provider: &ProviderKind,
    unsent_response: &str,
    frozen_chunk: &str,
) -> ProviderOutputVerdict {
    let whole = inspect_provider_streaming_output(provider, unsent_response);
    let frozen = inspect_provider_streaming_output(provider, frozen_chunk);
    match (whole, frozen) {
        (ProviderOutputVerdict::Blocked { kind }, _)
        | (_, ProviderOutputVerdict::Blocked { kind }) => ProviderOutputVerdict::Blocked { kind },
        (ProviderOutputVerdict::Hold { kind }, _) | (_, ProviderOutputVerdict::Hold { kind }) => {
            ProviderOutputVerdict::Hold { kind }
        }
        _ => ProviderOutputVerdict::Clean,
    }
}

pub(crate) fn safe_blocked_body(_kind: ProviderOutputKind) -> &'static str {
    BLOCKED_PROVIDER_OUTPUT_BODY
}

pub(crate) fn safe_held_body(_kind: ProviderOutputKind) -> &'static str {
    HELD_PROVIDER_OUTPUT_BODY
}

fn inspect_claude_prose(prose: &str) -> ProviderOutputVerdict {
    let canonical = canonical_control_form(prose);
    if PRIVATE_TASK_ANCHORS
        .iter()
        .any(|anchor| canonical.contains(&anchor.to_ascii_lowercase()))
    {
        return ProviderOutputVerdict::Blocked {
            kind: ProviderOutputKind::ClaudeSystemNotification,
        };
    }
    if tool_wrapper_compound_present(&canonical) {
        return ProviderOutputVerdict::Blocked {
            kind: ProviderOutputKind::ClaudeToolWrapper,
        };
    }
    if trailing_control_prefix(prose) {
        return ProviderOutputVerdict::Hold {
            kind: ProviderOutputKind::PartialControlMarker,
        };
    }
    ProviderOutputVerdict::Clean
}

fn tool_wrapper_compound_present(prose: &str) -> bool {
    (prose.contains("<invoke name=") && contains_parameter_token(prose))
        || tokens_separated_only_by_whitespace(prose, "</parameter>", "</invoke>")
}

fn contains_parameter_token(prose: &str) -> bool {
    prose.contains("</parameter>")
        || prose.match_indices("<parameter").any(|(index, token)| {
            prose[index + token.len()..]
                .chars()
                .next()
                .is_some_and(|next| next.is_whitespace() || matches!(next, '>' | '='))
        })
}

fn tokens_separated_only_by_whitespace(text: &str, first: &str, second: &str) -> bool {
    let mut remaining = text;
    while let Some(index) = remaining.find(first) {
        let after = &remaining[index + first.len()..];
        if after.trim_start().starts_with(second) {
            return true;
        }
        remaining = after;
    }
    false
}

fn contains_control_anchor(prose: &str) -> bool {
    contains_provider_control_anchor(prose)
}

fn trailing_control_prefix(prose: &str) -> bool {
    let canonical = canonical_control_form(prose);
    CONTROL_ANCHORS.iter().any(|anchor| {
        let anchor = anchor.to_ascii_lowercase();
        let max_len = canonical.len().min(anchor.len().saturating_sub(1));
        (1..=max_len)
            .rev()
            .any(|len| canonical.ends_with(&anchor[..len]))
    })
}

/// Canonicalizes only the representations that can conceal an already-known
/// control anchor: bounded entity unescaping to a fixed point, Unicode case
/// folding, and format/control removal. This does not turn a denylist into a
/// provenance boundary; structured-field handling remains a separate concern.
fn canonical_control_form(text: &str) -> String {
    let mut decoded = text.to_string();
    for _ in 0..8 {
        let next = crate::services::discord::tui_task_card::decode_entities_once(&decoded);
        if next == decoded {
            break;
        }
        decoded = next;
    }
    decoded
        .chars()
        .filter(|ch| !is_ignorable_control(*ch))
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_ignorable_control(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{061C}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{17B4}'
                | '\u{17B5}'
                | '\u{180E}'
                | '\u{200B}'
                | '\u{200C}'
                | '\u{200D}'
                | '\u{200E}'
                | '\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF0}'..='\u{FFF8}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_anchor_variants_block_and_footer_projection_discards_detail() {
        let variants = [
            "[SYSTEM NOTIFICATION - NOT USER INPUT] <OUTPUT-FILE>secret</OUTPUT-FILE>",
            "[SYSTEM NOTIFICATION - NOT USER INPUT] <output-file\u{200b}>secret</output-file\u{200b}>",
            "[SYSTEM NOTIFICATION - NOT USER INPUT] &amp;amp;lt;output-file&amp;amp;gt;SECRET&amp;amp;lt;/output-file&amp;amp;gt;",
        ];
        for detail in variants {
            assert!(matches!(
                inspect_provider_output(&ProviderKind::Claude, detail),
                ProviderOutputVerdict::Blocked { .. }
            ));
            let card = format!("✅ Task completed\n\n**done**\n\n{detail}");
            assert!(markdown_contains_provider_control_anchor(&card));
        }
    }

    #[test]
    fn canonical_anchor_normalization_does_not_block_normal_detail() {
        let detail = "Build completed; report is available in the public artifact.";
        assert_eq!(
            inspect_provider_output(&ProviderKind::Claude, detail),
            ProviderOutputVerdict::Clean
        );
        assert!(!markdown_contains_provider_control_anchor(detail));
    }
}

fn markdown_prose(text: &str) -> String {
    let mut prose = String::with_capacity(text.len());
    let mut code_block_depth = 0usize;
    for event in Parser::new_ext(text, Options::empty()) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => code_block_depth += 1,
            Event::End(Tag::CodeBlock(_)) => code_block_depth = code_block_depth.saturating_sub(1),
            Event::Code(_) => {}
            Event::Text(value) | Event::Html(value) if code_block_depth == 0 => {
                prose.push_str(&value);
            }
            Event::SoftBreak | Event::HardBreak if code_block_depth == 0 => prose.push('\n'),
            _ => {}
        }
    }
    prose
}
