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

pub(crate) struct Card {
    pub name: String,
    pub content: String,
}

/// The line a post of `request_id` carries and no other post does.
pub(crate) fn marker(request_id: Uuid) -> String {
    format!("-# decision {request_id}")
}

/// The card, or its length in UTF-16 units when Discord would refuse it as too long.
pub(crate) fn render(
    ask: &Ask,
    options: &[OptionRow],
    owner: Option<u64>,
    request_id: Uuid,
) -> Result<Card, usize> {
    let mut content = format!("## {}\n", ask.title.trim());
    if !ask.context.trim().is_empty() {
        content.push_str(&format!("{}\n", ask.context.trim()));
    }
    content.push_str("\n**선택지**\n");
    for (index, option) in options.iter().enumerate() {
        let key = (option.key.as_deref()).map_or(String::new(), |key| format!(" (`{key}`)"));
        content.push_str(&format!("{}. **{}**{key}", index + 1, option.label));
        if !option.detail.trim().is_empty() {
            content.push_str(&format!(" — {}", option.detail.trim()));
        }
        if let Some(percent) = option.recommendation_percent {
            content.push_str(&format!(" · Gemini 추천 {percent}%"));
        }
        content.push('\n');
    }
    if options
        .iter()
        .all(|option| option.recommendation_percent.is_none())
    {
        content.push_str("Gemini 추천: 추천 확인 불가\n");
    }
    let (agent, request_ref) = (&ask.agent_id, ask.request_ref.trim());
    content.push_str(&format!("요청: `{agent}` · ref `{request_ref}`\n"));
    let mention = owner.map_or(String::new(), |owner| format!("<@{owner}> "));
    content.push_str(&format!(
        "{mention}{NOT_YET} — 버튼은 선택 처리가 연결된 뒤에 열린다.\n{}",
        marker(request_id)
    ));
    let units = content.encode_utf16().count();
    if units > CONTENT_LIMIT {
        return Err(units);
    }
    let name = ask.title.trim().chars().take(100).collect();
    Ok(Card { name, content })
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
