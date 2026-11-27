//! Injected input uses the idle user header plus its attempt's nonce, ` · <nonce>]`; the durable
//! ledger identifies injected frames, tagged or untagged.

pub mod history;

const USER: &str = "[User: ";
/// The schema marker with whitespace removed, as wrapped composer rows still show it.
const SCHEMA_FOLDED: &str = "·📱v1mode=inject";

/// The fields of one durable-owned user header, as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header<'a> {
    pub name: &'a str,
    pub author: &'a str,
    /// Empty for a header written without an attempt tag.
    pub nonce: &'a str,
}

/// The name the idle `[User: …]` header shows: whitespace collapsed, brackets and braces made
/// parentheses, and `user <id>` when nothing is left.
pub(crate) fn author_name(name: &str, id: impl std::fmt::Display) -> String {
    let collapsed = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let base = if collapsed.is_empty() {
        format!("user {id}")
    } else {
        collapsed
    };
    let sanitized = base
        .chars()
        .map(|ch| match ch {
            '\r' | '\n' => ' ',
            '[' | '{' => '(',
            ']' | '}' => ')',
            _ => ch,
        })
        .collect::<String>();
    sanitized.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The name Discord shows for a person: the server nickname, else the global display name, else
/// the username.
pub fn display_name<'a>(
    nick: Option<&'a str>,
    global_name: Option<&'a str>,
    username: &'a str,
) -> &'a str {
    [nick, global_name]
        .into_iter()
        .flatten()
        .find(|name| !name.trim().is_empty())
        .unwrap_or(username)
}

pub fn idle(name: &str, author: &str, body: &str) -> String {
    framed(name, author, "", body)
}

fn framed(name: &str, author: &str, tag: &str, body: &str) -> String {
    let prefix = format!("[User: {} (ID: {author}){tag}]", author_name(name, author));
    let body = body.replace("\r\n", "\n").replace('\r', "\n");
    if body.is_empty() {
        prefix
    } else if body.contains('\n') {
        format!("{prefix}\n{body}")
    } else {
        format!("{prefix} {body}")
    }
}

/// A frame whose body is an approved Decision opinion, byte for byte. Only an event read from PG
/// that approved exactly this text builds one; a source name or request metadata cannot.
pub struct PreservedFrame(String);

impl PreservedFrame {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The idle frame without the line-ending rewrite, or `None` when the event did not approve `body`.
pub fn preserved_decision_body(
    event: &crate::db::decisions::DecisionProvenance,
    name: &str,
    body: &str,
) -> Option<PreservedFrame> {
    use crate::db::decisions::EventKind;
    if event.kind() != EventKind::Opinion || !event.approves(body) || body.is_empty() {
        return None;
    }
    let author = event.actor_id().to_string();
    let prefix = format!("[User: {} (ID: {author})]", author_name(name, &author));
    let gap = if body.contains(['\n', '\r']) {
        '\n'
    } else {
        ' '
    };
    Some(PreservedFrame(format!("{prefix}{gap}{body}")))
}

/// The idle frame tagged with the attempt's nonce, so two attempts of one text differ; a nonce
/// not shaped like `fresh_nonce`'s adds no tag.
pub fn plain_inject(_source: &str, author: &str, name: &str, nonce: &str, body: &str) -> String {
    let tag = if is_nonce(nonce) {
        format!(" · {nonce}")
    } else {
        String::new()
    };
    framed(name, author, &tag, body)
}

fn is_nonce(text: &str) -> bool {
    text.len() == 8 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A plain header's inside without its attempt tag, and the tag, empty when there is none.
pub fn untagged(inner: &str) -> (&str, &str) {
    match inner.rsplit_once(" · ") {
        Some((who, nonce)) if who.ends_with(')') && is_nonce(nonce) => (who, nonce),
        _ => (inner, ""),
    }
}

/// A durable-owned plain user header, tagged or not, with the bytes after its closing bracket.
/// Metadata is read from the right, so a name that looks like it never supplies it.
pub fn parse(text: &str) -> Option<(Header<'_>, &str)> {
    let rest = text.strip_prefix(USER)?;
    let close = rest.find(']')?;
    let (inner, after) = (&rest[..close], &rest[close + 1..]);
    let (who, nonce) = untagged(inner);
    let (name, author) = who.strip_suffix(')')?.rsplit_once(" (ID: ")?;
    if name.is_empty()
        || author.is_empty()
        || !author.bytes().all(|b| b.is_ascii_digit())
        || !history::known(text)
    {
        return None;
    }
    Some((
        Header {
            name,
            author,
            nonce,
        },
        after,
    ))
}

/// Whether `text` opens with a durable-owned plain frame.
pub(crate) fn opens_inject(text: &str) -> bool {
    text.starts_with(USER) && history::known(text)
}

/// Whether screen rows, however Claude wrapped them, open with an inject header; whitespace is
/// ignored so a header split across rows still counts.
pub(crate) fn shows_inject<S: AsRef<str>>(rows: &[S]) -> bool {
    let Some(first) = rows.first().map(|row| row.as_ref().trim_start()) else {
        return false;
    };
    if !first
        .get(..6)
        .is_some_and(|head| head.eq_ignore_ascii_case("[User:"))
    {
        return false;
    }
    let folded: String = rows
        .iter()
        .flat_map(|row| row.as_ref().chars())
        .filter(|c| !c.is_whitespace())
        .collect();
    folded
        .split(']')
        .next()
        .is_some_and(|header| header.contains(SCHEMA_FOLDED))
}

/// Whether an inject header shows anywhere in the rows, as the older `[📱 ` check looked for
/// it anywhere; a person's keys before it or a wrap inside it still count.
pub(crate) fn holds_inject<S: AsRef<str>>(rows: &[S]) -> bool {
    let folded: String = rows
        .iter()
        .flat_map(|row| row.as_ref().chars())
        .filter(|c| !c.is_whitespace())
        .collect();
    folded.contains(SCHEMA_FOLDED)
}

/// A row AgentDesk opened as a Discord prompt (`[User:` …), excluding a running turn's input.
pub(crate) fn opens_discord_prompt(row: &str) -> bool {
    row.get(..6)
        .is_some_and(|head| head.eq_ignore_ascii_case("[User:"))
        && !shows_inject(&[row])
}
