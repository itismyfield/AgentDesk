//! The starter post of a Decision request: the question as one message a reader understands on
//! its own, one button per option, a status line, and a marker line that finds the post again.

use poise::serenity_prelude as serenity;
use serde_json::{Value, json};
use uuid::Uuid;

use super::Ask;
use crate::db::decisions::AttemptState;
use crate::db::decisions::requests::OptionRow;

/// Discord's limit on message content, counted in UTF-16 code units.
pub(crate) const CONTENT_LIMIT: usize = 2000;

/// Shown while the buttons are disabled: before they are enabled, and for good on a kind of
/// target whose selection handling is not wired.
pub(crate) const NOT_YET: &str = "아직 선택을 받지 않음";
/// Shown while the buttons take a selection.
pub(crate) const OPEN: &str = "선택을 기다리는 중 — 아래 버튼 하나를 누른다.";

/// The status of a card whose choice is stored and being delivered.
pub(crate) fn selecting(label: &str) -> String {
    format!("선택됨: **{label}** — 세션에 전달하는 중.")
}

/// The status of a card whose selection's delivery ended as `state`.
pub(crate) fn ended(label: &str, state: AttemptState, detail: Option<&str>) -> String {
    let result = match state {
        AttemptState::Delivered => "세션에 전달됨.".to_string(),
        AttemptState::Failed => format!("세션에 전달하지 못함: {}", detail.unwrap_or("사유 없음")),
        _ => "전달 결과를 확인하지 못함(다시 보내지 않음).".to_string(),
    };
    format!("선택됨: **{label}** — {result}")
}

/// The posted card with its status line replaced. The body, the owner mention and the marker
/// line stay as posted, and a status too long for Discord's limit is cut, never the body.
pub(crate) fn with_status(content: &str, status: &str) -> String {
    let mut lines: Vec<&str> = content.lines().collect();
    let Some(at) = lines.len().checked_sub(2) else {
        return content.to_string();
    };
    let old = lines[at];
    let mention = (old.strip_prefix("<@"))
        .and_then(|rest| rest.find("> "))
        .map_or("", |end| &old[..end + 4]);
    let units = |text: &str| text.encode_utf16().count();
    let room = CONTENT_LIMIT.saturating_sub(units(content) - units(old) + units(mention));
    let mut used = 0;
    let fits = |c: &char| {
        used += c.len_utf16();
        used <= room
    };
    let line = format!(
        "{mention}{}",
        status.chars().take_while(fits).collect::<String>()
    );
    lines[at] = &line;
    lines.join("\n")
}

/// The starter's buttons for an edit: enabled or not, the stored choice marked.
pub(crate) fn buttons(
    options: &[(String, String, bool)],
    enabled: bool,
) -> Vec<serenity::CreateActionRow> {
    let button = |(label, custom_id, chosen): &(String, String, bool)| {
        let label: String = label.chars().take(80).collect();
        let style = match chosen {
            true => serenity::ButtonStyle::Success,
            false => serenity::ButtonStyle::Secondary,
        };
        let button = serenity::CreateButton::new(custom_id.as_str());
        button.label(label).style(style).disabled(!enabled)
    };
    vec![serenity::CreateActionRow::Buttons(
        options.iter().map(button).collect(),
    )]
}

/// The forum post: the card, a mention that may only reach the owner, and one disabled button
/// per option carrying that option's handle.
pub(crate) fn post_body(
    card: &Card,
    options: &[OptionRow],
    custom_ids: &[String],
    owner: Option<u64>,
) -> Value {
    let button = |(option, custom_id): (&OptionRow, &String)| {
        let label: String = option.label.chars().take(80).collect();
        json!({"type": 2, "style": 2, "label": label, "custom_id": custom_id, "disabled": true})
    };
    let buttons: Vec<Value> = options.iter().zip(custom_ids).map(button).collect();
    let users: Vec<String> = owner.iter().map(u64::to_string).collect();
    json!({"name": card.name, "message": {
        "content": card.content,
        "allowed_mentions": {"parse": [], "users": users},
        "components": [{"type": 1, "components": buttons}],
    }})
}
