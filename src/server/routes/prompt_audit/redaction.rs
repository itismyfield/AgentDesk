use regex::Regex;
use std::ops::Range;
use std::sync::LazyLock;

static SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"(?i)"#,
        r#"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)"#,
        r#"|\b(?:authorization|cookie|set-cookie)[ \t]*:[ \t]*(?:[^\r\n"']+(?:\r?\n[ \t]+[^\r\n"']+)*|(?:\r?\n[ \t]+[^\r\n"']+)+)"#,
        r#"|\b[a-z][a-z0-9+.-]*://[^\s/:@]+:[^\s/@]+@"#,
        r#"|\b(?:bearer|basic)[ \t]+[a-z0-9_./+~=-]+"#,
        r#"|(?:--(?:[a-z0-9]+[-_])*(?:key|token|secret|password|passwd)(?:[ \t]+|=)"#,
        r#"|\b[a-z0-9_-]*(?:key|token|secret|password|passwd|pwd|credentials?|authorization|cookie)["']?[ \t]*[:=][ \t]*)"#,
        r#"(?:\x22(?:\\.|[^\x22\\])*\x22|'(?:\\.|[^'\\])*'|[^\s"'\x60]+)"#,
        r#"|\b(?:gh[pousr]_[a-z0-9_]{4,}|github_pat_[a-z0-9_]{4,}|sk-[a-z0-9_-]{12,}|xox[a-z][-.][a-z0-9_.-]{8,}|AKIA[A-Z0-9]{16}|glpat-[a-z0-9_-]{12,})"#,
        r#"|\b(?:eyJ[a-z0-9_-]+\.[a-z0-9_-]+\.[a-z0-9_-]+|[a-z0-9_-]{23,28}\.[a-z0-9_-]{6}\.[a-z0-9_-]{25,}|mfa\.[a-z0-9_-]{40,})\b"#,
    ))
    .expect("valid audit credential patterns")
});

pub(super) fn ranges(input: &str) -> Vec<Range<usize>> {
    let mut matches = crate::utils::redact::registered_secret_ranges(input);
    matches.extend(SECRET.find_iter(input).map(|hit| hit.range()));
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
