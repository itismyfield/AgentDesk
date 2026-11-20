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

fn value_start(line: &str) -> Option<usize> {
    let keyed = KEY.captures_iter(line).filter_map(|hit| {
        if let Some(query) = hit.name("query") {
            match url::form_urlencoded::parse(query.as_str().as_bytes()).next() {
                Some((name, _)) if FIELD_NAME.is_match(&name) => {}
                Some((name, _)) if !name.contains(['%', '\u{fffd}']) => return None,
                _ => return Some(0),
            }
        }
        let end = hit.get(0).unwrap().end();
        // An escaped or quoted start can bind the apparent key to preceding text.
        let uncertain = line[end..].trim_start().starts_with(['"', '\'', '$', '\\']);
        Some(if uncertain { 0 } else { end })
    });
    keyed.chain(SECRET.find(line).map(|hit| hit.start())).min()
}

pub(super) fn ranges(input: &str) -> Vec<Range<usize>> {
    let mut matches: Vec<_> = with_registered_secret_ranges(input, |ranges| ranges.collect());
    matches.extend(PEM.find_iter(input).map(|hit| hit.range()));
    // Neither a blank line nor a quote count proves that a value ended, so masking runs to the end.
    let mut offset = 0;
    let start = input.split_inclusive('\n').find_map(|raw| {
        let base = offset;
        offset += raw.len();
        value_start(raw.trim_end_matches(['\r', '\n'])).map(|start| base + start)
    });
    matches.extend(start.map(|start| start..input.len()));
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
