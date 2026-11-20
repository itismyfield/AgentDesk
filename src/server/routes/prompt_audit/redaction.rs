use regex::Regex;
use std::ops::Range;
use std::sync::LazyLock;

use crate::utils::redact::with_registered_secret_ranges;

const FIELD: &str = r"[a-z0-9_-]*(?:key|token|secret|password|passwd|pwd|credentials?|authorization|cookie)(?:[-_](?:b64|base64))?";

static PEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
    )
    .expect("valid private key pattern")
});
static KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?ix)(?P<fold>
            ^[\x20\t]*(?:(?:[-*]|[0-9]+[.)])[\x20\t]+)*(?:\[[a-z0-9:]+\][\x20\t]+)?
            ["']?(?:authorization|proxy-authorization|cookie|set-cookie|x-api-key|(?P<setting>{FIELD}))["']?[\x20\t]*:
            |(?:^|[\x20\t;&|()<>])(?:--(?:header|{FIELD})(?:[\x20\t=]|$)|-(?-i:[A-Za-z]*?[Hb])|-p)
        )|(?:^|[\x20\t;{{,\x60"'(]){FIELD}["']?[\x20\t]*[:=]
         |[?&](?P<query>[a-z0-9_%+.-]+)="#
    )).expect("valid credential key pattern")
});
static FIELD_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?i)^{FIELD}$")).expect("valid credential field pattern")
});
static SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"(?i)\b[a-z][a-z0-9+.-]*://[^\s/:@]+:[^\s/@]+@"#,
        r#"|\b(?:bearer|basic)[ \t]+[a-z0-9_./+~=-]+"#,
        r#"|\b(?:gh[pousr]_[a-z0-9_]{4,}|github_pat_[a-z0-9_]{4,}|sk-[a-z0-9_-]{12,}|xox[a-z][-.][a-z0-9_.-]{8,}|AKIA[A-Z0-9]{16}|glpat-[a-z0-9_-]{12,})"#,
        r#"|\b(?:eyJ[a-z0-9_-]+\.[a-z0-9_-]+\.[a-z0-9_-]+|[a-z0-9_-]{23,28}\.[a-z0-9_-]{6}\.[a-z0-9_-]{25,}|mfa\.[a-z0-9_-]{40,})\b"#,
    )).expect("valid standalone credential patterns")
});

#[derive(Clone, Copy)]
enum Quote {
    Single,
    Double,
    AnsiSingle,
}

impl Quote {
    fn unclosed(mut value: &[u8]) -> Option<Self> {
        // Adjacent quoted fragments belong to the same shell word.
        loop {
            let (quote, body) = match value {
                [b'\'', body @ ..] => (Self::Single, body),
                [b'"', body @ ..] => (Self::Double, body),
                [b'$', b'\'', body @ ..] => (Self::AnsiSingle, body),
                []
                | [
                    b' ' | b'\t' | b';' | b'&' | b'|' | b'(' | b')' | b'<' | b'>',
                    ..,
                ] => {
                    return None;
                }
                [b'\\', _, rest @ ..] | [_, rest @ ..] => {
                    value = rest;
                    continue;
                }
            };
            let Some(close) = quote.close(body) else {
                return Some(quote);
            };
            value = &body[close..];
        }
    }

    fn close(self, body: &[u8]) -> Option<usize> {
        let (delimiter, escapes) = match self {
            Self::Single => (b'\'', false),
            Self::Double => (b'"', true),
            Self::AnsiSingle => (b'\'', true),
        };
        let mut bytes = body.iter().copied().enumerate();
        while let Some((index, byte)) = bytes.next() {
            if byte == delimiter {
                return Some(index + 1);
            }
            if escapes && byte == b'\\' {
                bytes.next();
            }
        }
        None
    }
}

#[derive(Default)]
struct Continuation {
    indent: Option<usize>,
    quotes: [Option<Quote>; 3],
    escaped: bool,
}

pub(super) fn ranges(input: &str) -> Vec<Range<usize>> {
    let mut matches: Vec<_> = with_registered_secret_ranges(input, |ranges| ranges.collect());
    matches.extend(PEM.find_iter(input).map(|hit| hit.range()));
    let mut offset = 0;
    let mut fold: Option<(usize, Continuation)> = None;
    for raw in input.split_inclusive('\n') {
        let base = offset;
        offset += raw.len();
        let line = raw.trim_end_matches(['\r', '\n']);
        let end = base + line.len();
        let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
        let blank = line.trim().is_empty();
        let mut scan = line;
        let mut next = Continuation::default();
        let mut joined = None;
        if let Some((index, previous)) = fold.take() {
            let quoted = previous.quotes.iter().any(Option::is_some);
            if previous.escaped
                || quoted
                || previous
                    .indent
                    .is_some_and(|limit| indent > limit && !blank)
            {
                joined = Some(index);
                next.indent = previous.indent;
                if quoted {
                    let mut resume = 0;
                    for quote in previous.quotes.into_iter().flatten() {
                        let open = if let Some(close) = quote.close(line.as_bytes()) {
                            resume = resume.max(close);
                            Quote::unclosed(&line.as_bytes()[close..])
                        } else {
                            Some(quote)
                        };
                        if let Some(quote) = open {
                            next.quotes[quote as usize] = Some(quote);
                            resume = line.len();
                        }
                    }
                    scan = &line[resume..];
                } else if previous.escaped
                    && let Some(quote) = Quote::unclosed(line.trim_start().as_bytes())
                {
                    next.quotes[quote as usize] = Some(quote);
                }
            } else if blank {
                fold = Some((index, previous));
                continue;
            }
        }

        let mut start = SECRET.find(scan).map(|hit| hit.start());
        for hit in KEY.captures_iter(scan) {
            let end = hit.get(0).unwrap().end();
            let value = scan[end..].trim_start();
            if let Some(query) = hit.name("query") {
                match url::form_urlencoded::parse(query.as_str().as_bytes()).next() {
                    Some((name, _)) if FIELD_NAME.is_match(&name) => {}
                    Some((name, _)) if !name.contains(['%', '\u{fffd}']) => continue,
                    _ => {
                        start = Some(0);
                        continue;
                    }
                }
            } else {
                if let Some(quote) = Quote::unclosed(value.as_bytes()) {
                    next.quotes[quote as usize] = Some(quote);
                }
                next.escaped |= value.ends_with('\\') && !value.contains(char::is_whitespace);
            }
            if hit.name("fold").is_some() {
                let limit = if hit.name("setting").is_some() {
                    indent
                } else {
                    0
                };
                next.indent = Some(next.indent.map_or(limit, |prior| prior.min(limit)));
            }
            let candidate = if value.starts_with(['"', '\'', '$', '\\']) {
                0
            } else {
                end
            };
            start = Some(start.map_or(candidate, |prior| prior.min(candidate)));
        }
        if let Some(index) = joined {
            matches[index].end = end;
        } else if let Some(start) = start {
            matches.push(base + start..end);
        } else {
            continue;
        }
        next.escaped |= line.ends_with('\\') && (joined.is_some() || next.indent.is_some());
        fold = Some((joined.unwrap_or(matches.len() - 1), next));
    }
    matches.retain(|span| !span.is_empty());
    matches.sort_unstable_by_key(|range| range.start);
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for matched in matches {
        if let Some(previous) = ranges.last_mut()
            && matched.start < previous.end
        {
            previous.end = previous.end.max(matched.end);
        } else {
            ranges.push(matched);
        }
    }
    ranges
}

pub(super) fn mask_range(
    input: &str,
    range: Range<usize>,
    secrets: &[Range<usize>],
) -> (String, usize) {
    let mut text = String::with_capacity(range.len());
    let mut count = 0;
    let mut cursor = range.start;
    for secret in secrets {
        if secret.start >= range.end {
            break;
        }
        if secret.end <= range.start {
            continue;
        }
        text.push_str(&input[cursor..secret.start.max(range.start)]);
        text.push_str("***");
        count += usize::from(secret.start >= range.start);
        cursor = secret.end.min(range.end);
    }
    text.push_str(&input[cursor..range.end]);
    (text, count)
}

pub(super) fn mask(input: &str) -> (String, usize) {
    mask_range(input, 0..input.len(), &ranges(input))
}
