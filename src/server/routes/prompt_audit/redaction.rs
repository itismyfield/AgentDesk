use regex::Regex;
use std::ops::Range;
use std::sync::LazyLock;

use crate::utils::redact::{
    CURL_COOKIE_OPTION_PATTERN, HEADER_OPTION_PATTERN, SHELL_WORD_FRAGMENT_PATTERN,
    registered_secret_ranges,
};

const FIELD: &str = r"[a-z0-9_-]*(?:key|token|secret|password|passwd|pwd|credentials?|authorization|cookie)(?:[-_](?:b64|base64))?";
const HEADER_NAME: &str = r"(?:authorization|proxy-authorization|cookie|set-cookie|x-api-key)";

static PEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
    )
    .expect("valid private key pattern")
});
static HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?im)^[ \t]*{HEADER_NAME}[ \t]*:[ \t]*(?:[^\r\n]+(?:\r?\n[ \t]+[^\r\n]+)*|(?:\r?\n[ \t]+[^\r\n]+)+)"
    )).expect("valid credential header pattern")
});
static SHELL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?im)(?:^|[ \t;&|()])(?P<option>(?:(?P<header>{HEADER_OPTION_PATTERN})|{CURL_COOKIE_OPTION_PATTERN}|--{FIELD}(?:[ \t]+|=)|-p[ \t=]*)(?P<argument>{SHELL_WORD_FRAGMENT_PATTERN}+))"
    )).expect("valid shell option pattern")
});
static HEADER_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?i)^{HEADER_NAME}[ \t]*:")).expect("valid credential header prefix")
});
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?im)(?:^|[ \t;{{,\x60"'(])(?P<assignment>{FIELD}["']?[ \t]*[:=][ \t]*(?:\x22(?:\\.|[^\x22\\])*\x22|'(?:\\.|[^'\\])*'|[^\s"'\x60]+))"#
    )).expect("valid credential assignment pattern")
});
static SETTING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?im)^([ \t]*)["']?{FIELD}["']?[ \t]*:[^\r\n]*\r?\n"#
    ))
    .expect("valid credential setting pattern")
});
static QUERY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"[?&]([^#&=\s"'<>()]+)=([^#&\s"'<>()]+)"#).expect("valid query pattern")
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

fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

pub(super) fn ranges(input: &str) -> Vec<Range<usize>> {
    let mut matches = registered_secret_ranges(input);
    // Each detector sees the original bytes, including prefixes consumed by other detectors.
    for detector in [&*PEM, &*HEADER, &*SECRET] {
        matches.extend(detector.find_iter(input).map(|hit| hit.range()));
    }
    matches.extend(
        ASSIGNMENT
            .captures_iter(input)
            .map(|hit| hit.name("assignment").unwrap().range()),
    );
    matches.extend(SHELL.captures_iter(input).filter_map(|hit| {
        let argument = hit
            .name("argument")?
            .as_str()
            .trim_start_matches(['\'', '"', '$', ' ', '\t']);
        (hit.name("header").is_none() || HEADER_PREFIX.is_match(argument))
            .then(|| hit.name("option").unwrap().range())
    }));
    matches.extend(SETTING.captures_iter(input).filter_map(|hit| {
        let setting = hit.get(0)?;
        let key_indent = hit.get(1)?.len();
        let len = input[setting.end()..]
            .split_inclusive('\n')
            .take_while(|line| line.trim().is_empty() || indent(line) > key_indent)
            .map(str::len)
            .sum::<usize>();
        let end = setting.end() + len;
        (!input[setting.end()..end].trim().is_empty()).then_some(setting.start()..end)
    }));
    matches.extend(QUERY.captures_iter(input).filter_map(|hit| {
        let key = hit.get(1)?.as_str();
        let (decoded, _) = url::form_urlencoded::parse(key.as_bytes()).next()?;
        FIELD_NAME
            .is_match(&decoded)
            .then(|| hit.get(2).unwrap().range())
    }));
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
