//! Injected input uses the idle user header plus its attempt's nonce, ` · <nonce>]`; the durable
//! ledger identifies injected frames, tagged or untagged.

pub(crate) mod history;

const USER: &str = "[User: ";

/// The fields of one durable-owned user header, as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header<'a> {
    pub(crate) name: &'a str,
    pub(crate) author: &'a str,
    /// Empty for a header written without an attempt tag.
    pub(crate) nonce: &'a str,
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
pub(crate) fn display_name<'a>(
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

pub(crate) fn idle(name: &str, author: &str, body: &str) -> String {
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
pub(crate) fn plain_inject(
    _source: &str,
    author: &str,
    name: &str,
    nonce: &str,
    body: &str,
) -> String {
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
pub(crate) fn untagged(inner: &str) -> (&str, &str) {
    match inner.rsplit_once(" · ") {
        Some((who, nonce)) if who.ends_with(')') && is_nonce(nonce) => (who, nonce),
        _ => (inner, ""),
    }
}

/// A durable-owned plain user header, tagged or not, with the bytes after its closing bracket.
/// Metadata is read from the right, so a name that looks like it never supplies it.
pub(crate) fn parse(text: &str) -> Option<(Header<'_>, &str)> {
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

/// A row AgentDesk opened as a Discord prompt (`[User:` …).
pub(crate) fn opens_discord_prompt(row: &str) -> bool {
    row.get(..6)
        .is_some_and(|head| head.eq_ignore_ascii_case("[User:"))
}
