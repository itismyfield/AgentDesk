//! Ordinary user headers are shared by idle and injected input; the durable ledger identifies
//! injected frames. The earlier v1 and `[📱 …]` formats remain readable.

pub(crate) mod history;

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

pub(crate) fn idle(name: &str, author: &str, body: &str) -> String {
    let prefix = format!("[User: {} (ID: {author})]", author_name(name, author));
    let body = body.replace("\r\n", "\n").replace('\r', "\n");
    if body.is_empty() {
        prefix
    } else if body.contains('\n') {
        format!("{prefix}\n{body}")
    } else {
        format!("{prefix} {body}")
    }
}

pub(crate) fn plain_inject(
    _source: &str,
    author: &str,
    name: &str,
    _id: &str,
    body: &str,
) -> String {
    idle(name, author, body)
}

/// The v1 formatter remains only for reconstructing already written frames.

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

/// Whether `text` opens with the pre-v1 frame `legacy_inject` wrote: a source, a numeric author
/// and an 8-hex nonce.
pub(crate) fn opens_legacy_inject(text: &str) -> bool {
    let Some((header, _)) = text.trim_start().strip_prefix("[📱 ").and_then(|rest| {
        rest.split_once("]\n")
            .or_else(|| rest.strip_suffix(']').map(|header| (header, "")))
    }) else {
        return false;
    };
    let mut fields = header.rsplitn(3, " · ");
    let (Some(nonce), Some(author), Some(source)) = (fields.next(), fields.next(), fields.next())
    else {
        return false;
    };
    nonce.len() == 8
        && nonce
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && !author.is_empty()
        && author.bytes().all(|b| b.is_ascii_digit())
        && !source.trim().is_empty()
}

/// A durable-owned plain user header or a v1 header, with the bytes after its closing bracket.
/// V1 metadata is read from the right, so a name that looks like metadata never supplies it.
pub(crate) fn parse(text: &str) -> Option<(Header<'_>, &str)> {
    let rest = text.strip_prefix(USER)?;
    let close = rest.find(']')?;
    let (inner, after) = (&rest[..close], &rest[close + 1..]);
    if !inner.contains(SCHEMA) {
        let (name, author) = inner.strip_suffix(')')?.rsplit_once(" (ID: ")?;
        if name.is_empty()
            || author.is_empty()
            || !author.bytes().all(|b| b.is_ascii_digit())
            || !history::known(text)
        {
            return None;
        }
        return Some((
            Header {
                name,
                author,
                mode: Mode::Inject,
                source: "",
                nonce: "",
            },
            after,
        ));
    }
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

/// Whether `text` opens with a durable-owned plain frame or an injected or damaged v1 header.
pub(crate) fn opens_inject(text: &str) -> bool {
    let Some(rest) = text.strip_prefix(USER) else {
        return false;
    };
    let header = rest.split(']').next().unwrap_or_default();
    history::known(text)
        || header.contains(SCHEMA)
            && !parse(text).is_some_and(|(header, _)| header.mode == Mode::New)
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
