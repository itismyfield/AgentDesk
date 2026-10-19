//! The `[User: <name> (ID: <id>) · 📱 v1 mode=<new|inject> source=<source> · <nonce>]` header on
//! input pasted into a running turn, and its one shared reader; the older `[📱 …]` is still read.

const USER: &str = "[User: ";
const SCHEMA: &str = " · 📱 v1 mode=";
/// The schema marker with whitespace removed, as wrapped composer rows still show it.
const SCHEMA_FOLDED: &str = "·📱v1mode=inject";
/// Longest name a header keeps, in characters.
const MAX_NAME_CHARS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A new turn's prompt.
    New,
    /// Input that joins the turn already running.
    Inject,
}

/// The fields of one v1 header, as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header<'a> {
    pub(crate) name: &'a str,
    pub(crate) author: &'a str,
    pub(crate) mode: Mode,
    pub(crate) source: &'a str,
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

/// The idle name with no control character or `·`, cut to a bounded length; applying it twice
/// changes nothing, so a header read back rebuilds to the same bytes.
fn header_name(name: &str, author: &str) -> String {
    let plain: String = name
        .chars()
        .map(|c| match c {
            '·' => '.',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let name: String = author_name(&plain, author)
        .chars()
        .take(MAX_NAME_CHARS)
        .collect();
    name.trim_end().to_string()
}

/// The source as one token: bytes outside a plain set are percent-encoded.
fn source_key(source: &str) -> String {
    let mut key = String::with_capacity(source.len());
    for byte in source.bytes() {
        if byte.is_ascii_alphanumeric() || b":._-/@+".contains(&byte) {
            key.push(char::from(byte));
        } else {
            key.push_str(&format!("%{byte:02X}"));
        }
    }
    key
}

/// The input as pasted into a running turn; the header ends with ` · <nonce>]` as before.
pub(crate) fn inject(source: &str, author: &str, name: &str, nonce: &str, body: &str) -> String {
    let id: String = author
        .chars()
        .map(|c| if c.is_control() || c == ']' { ' ' } else { c })
        .collect();
    format!(
        "{USER}{} (ID: {id}){SCHEMA}inject source={} · {nonce}]\n{body}",
        header_name(name, author),
        source_key(source),
    )
}

/// The frame inputs were pasted with before v1, rebuilt only to read old transcripts.
pub(crate) fn legacy_inject(source: &str, author: &str, nonce: &str, body: &str) -> String {
    let clean = |value: &str| -> String {
        value
            .chars()
            .map(|c| if c.is_control() || c == ']' { ' ' } else { c })
            .collect()
    };
    format!(
        "[📱 {} · {} · {nonce}]\n{body}",
        clean(source),
        clean(author)
    )
}

/// The v1 header `text` opens with and what follows its closing bracket. The fields are read
/// from the right, so a name that looks like metadata never supplies it.
pub(crate) fn parse(text: &str) -> Option<(Header<'_>, &str)> {
    let rest = text.strip_prefix(USER)?;
    let close = rest.find(']')?;
    let (inner, after) = (&rest[..close], &rest[close + 1..]);
    let (meta, nonce) = inner.rsplit_once(" · ")?;
    let hex = |b: u8| matches!(b, b'0'..=b'9' | b'a'..=b'f');
    if nonce.len() != 8 || !nonce.bytes().all(hex) {
        return None;
    }
    let (who, meta) = meta.rsplit_once(SCHEMA)?;
    let (mode, source) = meta.split_once(" source=")?;
    let mode = match mode {
        "new" => Mode::New,
        "inject" => Mode::Inject,
        _ => return None,
    };
    let (name, author) = who.strip_suffix(')')?.rsplit_once(" (ID: ")?;
    let digits = !author.is_empty() && author.bytes().all(|b| b.is_ascii_digit());
    if name.is_empty() || !digits || source.contains(char::is_whitespace) {
        return None;
    }
    let header = Header {
        name,
        author,
        mode,
        source,
        nonce,
    };
    Some((header, after))
}

/// Whether `text` opens with a v1 header that is not a well-formed new-turn one: an inject
/// header, or a damaged header no reader may take for a new turn's prompt.
pub(crate) fn opens_inject(text: &str) -> bool {
    let Some(rest) = text.strip_prefix(USER) else {
        return false;
    };
    let header = rest.split(']').next().unwrap_or_default();
    header.contains(SCHEMA) && !parse(text).is_some_and(|(header, _)| header.mode == Mode::New)
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
