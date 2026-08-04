//! Tool output efficiency middleware.
//!
//! Detects oversized tool outputs and projects a bounded representation for
//! relay-facing transcripts. Provider source data remains untouched.
//!
//! Thresholds default to 100 lines or 8 KiB — whichever fires first. These
//! are conservative values intended to catch "full-file read" and "huge grep
//! dump" anti-patterns without false-positive on typical compiler/test output
//! (which tends to stay under a few KiB per tool call).
//!
use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};

/// Line-count threshold. Tool outputs with more than this many `\n`-separated
/// lines are flagged as oversize. Chosen to align with the shared prompt rule
/// ("10줄 이상 출력은 summary로") with a 10x safety margin so only genuinely
/// large dumps fire the warning.
pub const OVERSIZE_LINE_THRESHOLD: usize = 100;

/// Byte-count threshold. 8 KiB is roughly one Discord embed description cap
/// and covers the common "cat a whole source file" case.
pub const OVERSIZE_BYTE_THRESHOLD: usize = 8 * 1024;

/// Truncation marker string appended to the warn payload so log scrapers and
/// downstream pipelines can grep for oversized outputs without reparsing.
pub const TRUNCATION_MARKER: &str = "[tool_output_oversize:truncated]";
const ERROR_EDGE_BYTES: usize = 1024;
const ERROR_HEADER_CONTEXT_BYTES: usize = 64 * 1024;

static OVERSIZE_COUNT: AtomicU64 = AtomicU64::new(0);
static TOTAL_OUTPUT_BYTES: AtomicU64 = AtomicU64::new(0);
static TOTAL_OUTPUT_SAMPLES: AtomicU64 = AtomicU64::new(0);

/// Result of inspecting a tool output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OversizeReport {
    pub lines: usize,
    pub bytes: usize,
    pub oversize: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayOutputDisposition {
    Preserve,
    OmitBulk,
    SummarizeError,
}

impl RelayOutputDisposition {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Preserve => "preserve",
            Self::OmitBulk => "omit_bulk",
            Self::SummarizeError => "summarize_error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayOutputProjection<'a> {
    pub content: Cow<'a, str>,
    pub report: OversizeReport,
    pub disposition: RelayOutputDisposition,
}

/// Count `\n`-separated lines, handling both `\n` and `\r\n` uniformly. An
/// empty string is zero lines; trailing newline does not add a phantom line.
fn count_lines(content: &str) -> usize {
    if content.is_empty() {
        return 0;
    }
    let mut lines = content.bytes().filter(|b| *b == b'\n').count();
    // A non-terminated final line still counts as a line of output.
    if !content.ends_with('\n') {
        lines += 1;
    }
    lines
}

/// Inspect a tool output without mutating it. Returns the raw measurement;
/// callers that want logging + counters should prefer `observe`.
pub fn inspect(content: &str) -> OversizeReport {
    let lines = count_lines(content);
    let bytes = content.len();
    let oversize = lines > OVERSIZE_LINE_THRESHOLD || bytes > OVERSIZE_BYTE_THRESHOLD;
    OversizeReport {
        lines,
        bytes,
        oversize,
    }
}

fn utf8_head(content: &str, max_bytes: usize) -> &str {
    let mut end = content.len().min(max_bytes);
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    &content[..end]
}

fn utf8_tail(content: &str, max_bytes: usize) -> &str {
    let mut start = content.len().saturating_sub(max_bytes);
    while !content.is_char_boundary(start) {
        start += 1;
    }
    &content[start..]
}

fn bounded_redacted_error_tail(content: &str, max_bytes: usize) -> (String, usize) {
    let raw_tail = utf8_tail(content, max_bytes);
    let tail_start = content.len().saturating_sub(raw_tail.len());
    let mut safe_tail = raw_tail;
    let mut skipped = 0;

    // A tail that begins in the middle of a physical line can be retained when
    // its borrowed prefix proves it is not a sensitive header. The prefix-only
    // scan is allocation-free; if a Cookie/Authorization header or obs-fold
    // continuation began in the omitted region, fail closed by dropping the
    // partial line.
    if tail_start > 0 && content.as_bytes().get(tail_start.wrapping_sub(1)) != Some(&b'\n') {
        // Reuse the UTF-8-aware tail helper for the borrowed context window.
        // Subtracting a byte budget directly can land inside a multibyte code
        // point even though `tail_start` itself is a valid boundary.
        let context = utf8_tail(&content[..tail_start], ERROR_HEADER_CONTEXT_BYTES);
        let context_start = tail_start.saturating_sub(context.len());
        let newline = context.rfind('\n');
        let has_complete_line_prefix = context_start == 0 || newline.is_some();
        let line_prefix = newline.map_or(context, |position| &context[position + 1..]);
        let may_continue_sensitive_header = !has_complete_line_prefix
            || line_prefix.starts_with([' ', '\t'])
            || crate::utils::redact::contains_sensitive_header_prefix(line_prefix);
        if may_continue_sensitive_header {
            if let Some(line_end) = safe_tail.find('\n') {
                skipped += line_end + 1;
                safe_tail = &safe_tail[line_end + 1..];
            } else {
                return (
                    "[… truncated partial line redacted …]".to_string(),
                    raw_tail.len(),
                );
            }
        }
    }

    // Leading indented physical lines may be obs-fold continuations of a
    // sensitive header that began in the omitted region. Fail closed until the
    // first non-continuation line, then apply the normal redactor to that bounded
    // suffix only.
    while safe_tail.starts_with([' ', '\t']) {
        if let Some(line_end) = safe_tail.find('\n') {
            skipped += line_end + 1;
            safe_tail = &safe_tail[line_end + 1..];
        } else {
            skipped += safe_tail.len();
            safe_tail = "";
            break;
        }
    }

    let redacted =
        crate::services::discord::formatting::redact_sensitive_for_placeholder(safe_tail);
    if skipped == 0 {
        (redacted, 0)
    } else if redacted.is_empty() {
        ("[… truncated partial line redacted …]".to_string(), skipped)
    } else {
        (
            format!("[… truncated partial line redacted …]\n{redacted}"),
            skipped,
        )
    }
}

fn normalized_tool_key(name: &str) -> String {
    name.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn preserves_completion_output(tool_name: Option<&str>) -> bool {
    tool_name.is_some_and(|name| {
        matches!(
            normalized_tool_key(name).as_str(),
            "task"
                | "agent"
                | "taskcreate"
                | "taskupdate"
                | "tasklist"
                | "taskget"
                | "taskoutput"
                | "taskstop"
        )
    })
}

pub(crate) fn matched_or_last_tool<'a>(
    matched: Option<&'a str>,
    fallback: Option<&'a str>,
) -> Option<&'a str> {
    matched.or(fallback)
}

/// Produce the relay representation without changing the provider transcript.
/// Successful low-value bulk output is metadata-only, while Task/Agent family
/// completions remain intact because their payload is the delegated work result.
/// Errors retain small, redacted UTF-8 head/tail excerpts.
pub fn project_for_relay<'a>(
    tool_name: Option<&str>,
    is_error: bool,
    content: &'a str,
) -> RelayOutputProjection<'a> {
    let report = inspect(content);
    if !report.oversize || (!is_error && preserves_completion_output(tool_name)) {
        return RelayOutputProjection {
            content: Cow::Borrowed(content),
            report,
            disposition: RelayOutputDisposition::Preserve,
        };
    }
    if !is_error {
        return RelayOutputProjection {
            content: Cow::Owned(format!(
                "{TRUNCATION_MARKER} omitted success output (bytes={}, lines={})",
                report.bytes, report.lines
            )),
            report,
            disposition: RelayOutputDisposition::OmitBulk,
        };
    }

    // Keep both allocation and regex work bounded. Line-heavy errors below the
    // combined edge budget are sanitized once without duplicating overlapping
    // head/tail slices. Larger errors use a trusted head plus a tail that drops
    // an untrusted partial first line and leading obs-fold continuations.
    let excerpt = if report.bytes <= ERROR_EDGE_BYTES * 2 {
        crate::services::discord::formatting::redact_sensitive_for_placeholder(content)
    } else {
        let raw_head = utf8_head(content, ERROR_EDGE_BYTES);
        let head = crate::services::discord::formatting::redact_sensitive_for_placeholder(raw_head);
        let raw_tail = utf8_tail(content, ERROR_EDGE_BYTES);
        let (tail, skipped_tail_bytes) = bounded_redacted_error_tail(content, ERROR_EDGE_BYTES);
        let omitted = report
            .bytes
            .saturating_sub(raw_head.len().saturating_add(raw_tail.len()))
            .saturating_add(skipped_tail_bytes);
        format!("{head}\n… {omitted} bytes omitted …\n{tail}")
    };
    RelayOutputProjection {
        content: Cow::Owned(format!(
            "{TRUNCATION_MARKER} summarized error (bytes={}, lines={})\n{excerpt}",
            report.bytes, report.lines
        )),
        report,
        disposition: RelayOutputDisposition::SummarizeError,
    }
}

/// Inspect a tool output, update global counters, and emit a
/// `tracing::warn!` when the oversize thresholds fire. Returns the report
/// so the caller can optionally attach it to transcripts or DM surfaces.
///
/// `tool_name` is the originating tool (e.g. "Bash", "Read"). `is_error` is
/// propagated into the warn payload so dashboards can distinguish oversize
/// error dumps from oversize successful reads.
pub fn observe_projection(
    tool_name: Option<&str>,
    is_error: bool,
    projection: &RelayOutputProjection<'_>,
) {
    let report = projection.report;
    TOTAL_OUTPUT_BYTES.fetch_add(report.bytes as u64, Ordering::Relaxed);
    TOTAL_OUTPUT_SAMPLES.fetch_add(1, Ordering::Relaxed);
    if report.oversize {
        OVERSIZE_COUNT.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            target: "tool_output_oversize",
            tool = tool_name.unwrap_or("<unknown>"),
            is_error,
            output_len = report.bytes,
            rendered_len = projection.content.len(),
            line_count = report.lines,
            disposition = projection.disposition.as_str(),
            byte_threshold = OVERSIZE_BYTE_THRESHOLD,
            line_threshold = OVERSIZE_LINE_THRESHOLD,
            marker = TRUNCATION_MARKER,
            "tool output projected to a bounded relay representation"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_is_preserved_exactly() {
        let projection = project_for_relay(Some("Read"), false, "small\noutput");
        assert_eq!(projection.disposition, RelayOutputDisposition::Preserve);
        assert_eq!(projection.content, "small\noutput");
    }

    #[test]
    fn large_success_is_metadata_only() {
        let raw = format!("secret-payload-{}", "x".repeat(OVERSIZE_BYTE_THRESHOLD));
        let projection = project_for_relay(Some("Read"), false, &raw);
        assert_eq!(projection.disposition, RelayOutputDisposition::OmitBulk);
        assert!(projection.content.contains(TRUNCATION_MARKER));
        assert!(!projection.content.contains("secret-payload"));
        assert!(projection.content.len() < 256);
    }

    #[test]
    fn large_task_family_success_is_preserved_exactly() {
        let raw = format!("delegated-result-{}", "x".repeat(OVERSIZE_BYTE_THRESHOLD));
        for tool_name in ["Task", "agent", "task_output"] {
            let projection = project_for_relay(Some(tool_name), false, &raw);
            assert_eq!(projection.disposition, RelayOutputDisposition::Preserve);
            assert_eq!(projection.content, raw);
        }
    }

    #[test]
    fn interleaved_result_prefers_matched_task_over_latest_read() {
        let raw = format!("delegated-result-{}", "x".repeat(OVERSIZE_BYTE_THRESHOLD));
        let matched_task = matched_or_last_tool(Some("Task"), Some("Read"));
        let task = project_for_relay(matched_task, false, &raw);
        assert_eq!(task.disposition, RelayOutputDisposition::Preserve);
        assert_eq!(task.content, raw);

        for read in [
            matched_or_last_tool(Some("Read"), Some("Task")),
            matched_or_last_tool(None, Some("Read")),
        ] {
            let projection = project_for_relay(read, false, &raw);
            assert_eq!(projection.disposition, RelayOutputDisposition::OmitBulk);
        }
    }

    #[test]
    fn large_error_is_bounded_redacted_and_keeps_utf8_edges() {
        let raw = format!(
            "시작 Bearer top-secret {} password=hunter2 끝🙂",
            "가".repeat(OVERSIZE_BYTE_THRESHOLD)
        );
        let projection = project_for_relay(Some("Bash"), true, &raw);
        assert_eq!(
            projection.disposition,
            RelayOutputDisposition::SummarizeError
        );
        assert!(projection.content.len() < 2600);
        assert!(projection.content.contains("시작 Bearer ***"));
        assert!(projection.content.contains("password=*** 끝🙂"));
        assert!(!projection.content.contains("top-secret"));
        assert!(!projection.content.contains("hunter2"));
    }

    #[test]
    fn large_error_redacts_cookie_before_extracting_head_and_tail() {
        let cookie_value = "boundary-cookie-secret-".repeat(420);
        let raw = format!(
            "{}\nCookie: {cookie_value}\nvisible tail {}",
            "x".repeat(ERROR_EDGE_BYTES - 16),
            "y".repeat(64)
        );

        let projection = project_for_relay(Some("Bash"), true, &raw);

        assert_eq!(
            projection.disposition,
            RelayOutputDisposition::SummarizeError
        );
        assert!(projection.content.contains("Cookie: ***"));
        assert!(projection.content.contains("visible tail"));
        assert!(!projection.content.contains("boundary-cookie-secret"));
    }

    #[test]
    fn very_large_single_line_error_keeps_projection_memory_bounded() {
        let raw = format!("Cookie: {}", "bulk-secret".repeat(100_000));

        let projection = project_for_relay(Some("Bash"), true, &raw);

        assert_eq!(
            projection.disposition,
            RelayOutputDisposition::SummarizeError
        );
        assert!(projection.content.len() < ERROR_EDGE_BYTES * 2 + 512);
        assert!(projection.content.contains("Cookie: ***"));
        assert!(
            projection
                .content
                .contains("truncated partial line redacted")
        );
        assert!(!projection.content.contains("bulk-secret"));
    }

    #[test]
    fn very_large_non_ascii_error_aligns_context_window_to_utf8() {
        // 64 KiB before a valid tail boundary falls inside one of these
        // three-byte code points. The bounded context scan must align forward
        // instead of panicking while slicing the borrowed input.
        let raw = "€".repeat(30_000);

        let projection = project_for_relay(Some("Bash"), true, &raw);

        assert_eq!(
            projection.disposition,
            RelayOutputDisposition::SummarizeError
        );
        assert!(projection.content.contains(TRUNCATION_MARKER));
        assert!(projection.content.len() < ERROR_EDGE_BYTES * 2 + 512);
    }

    #[test]
    fn line_heavy_small_error_is_sanitized_once_without_overlapping_edges() {
        let raw = (0..=OVERSIZE_LINE_THRESHOLD)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(raw.len() < ERROR_EDGE_BYTES * 2);

        let projection = project_for_relay(Some("Bash"), true, &raw);

        assert_eq!(
            projection.disposition,
            RelayOutputDisposition::SummarizeError
        );
        assert_eq!(projection.content.matches("line-0\n").count(), 1);
        assert!(projection.content.ends_with("line-100"));
        assert!(!projection.content.contains("bytes omitted"));
    }
}
