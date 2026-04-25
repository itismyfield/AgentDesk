//! Tool output efficiency middleware.
//!
//! Introduced by issue #1084 (Epic 908-2). Detects oversized tool outputs
//! (e.g. `Bash`, `Read`) and emits a `tracing::warn!` with a truncation marker
//! plus observability counters so we can measure the average per-turn output
//! size. The actual auto-summary happens on the LLM side via the shared
//! role prompt (`_shared.prompt.md`); this module only flags + measures.
//!
//! Thresholds default to 100 lines or 8 KiB — whichever fires first. These
//! are conservative values intended to catch "full-file read" and "huge grep
//! dump" anti-patterns without false-positive on typical compiler/test output
//! (which tends to stay under a few KiB per tool call).
//!
//! The middleware never mutates the tool output itself; it only observes.
//! Downstream transcript + display code still receives the raw content so
//! we do not accidentally hide signal from the agent or from humans who
//! later inspect the session.

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

/// Inspect a tool output, update global counters, and emit a
/// `tracing::warn!` when the oversize thresholds fire. Returns the report
/// so the caller can optionally attach it to transcripts or DM surfaces.
///
/// `tool_name` is the originating tool (e.g. "Bash", "Read"). `is_error` is
/// propagated into the warn payload so dashboards can distinguish oversize
/// error dumps from oversize successful reads.
pub fn observe(tool_name: Option<&str>, is_error: bool, content: &str) -> OversizeReport {
    let report = inspect(content);
    TOTAL_OUTPUT_BYTES.fetch_add(report.bytes as u64, Ordering::Relaxed);
    TOTAL_OUTPUT_SAMPLES.fetch_add(1, Ordering::Relaxed);
    if report.oversize {
        OVERSIZE_COUNT.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            target: "tool_output_oversize",
            tool = tool_name.unwrap_or("<unknown>"),
            is_error,
            output_len = report.bytes,
            line_count = report.lines,
            byte_threshold = OVERSIZE_BYTE_THRESHOLD,
            line_threshold = OVERSIZE_LINE_THRESHOLD,
            marker = TRUNCATION_MARKER,
            "tool_output_oversize detected: consider summary instead of full dump"
        );
    }
    report
}

/// Snapshot of global counters. Used by `/api/analytics/observability` and
/// tests. Returned as a plain struct so callers do not depend on atomics.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolOutputCounters {
    pub tool_output_oversize_count: u64,
    pub total_output_bytes: u64,
    pub total_output_samples: u64,
    /// Convenience: mean bytes per observed tool output. Returns 0 when no
    /// samples have been recorded yet.
    pub avg_output_bytes: u64,
}

pub fn snapshot_counters() -> ToolOutputCounters {
    let oversize = OVERSIZE_COUNT.load(Ordering::Relaxed);
    let bytes = TOTAL_OUTPUT_BYTES.load(Ordering::Relaxed);
    let samples = TOTAL_OUTPUT_SAMPLES.load(Ordering::Relaxed);
    let avg = if samples == 0 { 0 } else { bytes / samples };
    ToolOutputCounters {
        tool_output_oversize_count: oversize,
        total_output_bytes: bytes,
        total_output_samples: samples,
        avg_output_bytes: avg,
    }
}

#[cfg(test)]
pub fn reset_counters_for_tests() {
    OVERSIZE_COUNT.store(0, Ordering::Relaxed);
    TOTAL_OUTPUT_BYTES.store(0, Ordering::Relaxed);
    TOTAL_OUTPUT_SAMPLES.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_is_not_flagged() {
        let report = inspect("hello\nworld\n");
        assert!(!report.oversize);
        assert_eq!(report.lines, 2);
        assert_eq!(report.bytes, "hello\nworld\n".len());
    }

    #[test]
    fn empty_output_is_zero() {
        let report = inspect("");
        assert!(!report.oversize);
        assert_eq!(report.lines, 0);
        assert_eq!(report.bytes, 0);
    }

    #[test]
    fn single_line_without_trailing_newline_counts_as_one() {
        let report = inspect("just one line");
        assert_eq!(report.lines, 1);
        assert!(!report.oversize);
    }

    #[test]
    fn line_threshold_triggers_oversize() {
        // OVERSIZE_LINE_THRESHOLD + 1 newlines → one more line than allowed
        let content: String = "x\n".repeat(OVERSIZE_LINE_THRESHOLD + 1);
        let report = inspect(&content);
        assert!(
            report.oversize,
            "expected oversize at {} lines",
            report.lines
        );
        assert!(report.lines > OVERSIZE_LINE_THRESHOLD);
    }

    #[test]
    fn line_threshold_at_exact_limit_is_not_oversize() {
        let content: String = "x\n".repeat(OVERSIZE_LINE_THRESHOLD);
        let report = inspect(&content);
        assert!(!report.oversize);
        assert_eq!(report.lines, OVERSIZE_LINE_THRESHOLD);
    }

    #[test]
    fn byte_threshold_triggers_oversize() {
        // One long line, no newlines, exceeding the byte threshold.
        let content = "a".repeat(OVERSIZE_BYTE_THRESHOLD + 1);
        let report = inspect(&content);
        assert!(report.oversize);
        assert_eq!(report.lines, 1);
        assert!(report.bytes > OVERSIZE_BYTE_THRESHOLD);
    }

    #[test]
    fn observe_increments_counters_and_totals() {
        reset_counters_for_tests();
        let small = "ok\n";
        observe(Some("Bash"), false, small);
        let big = "x\n".repeat(OVERSIZE_LINE_THRESHOLD + 5);
        observe(Some("Read"), false, &big);

        let snap = snapshot_counters();
        assert_eq!(snap.tool_output_oversize_count, 1);
        assert_eq!(snap.total_output_samples, 2);
        assert_eq!(
            snap.total_output_bytes,
            small.len() as u64 + big.len() as u64
        );
        assert!(snap.avg_output_bytes > 0);
    }

    #[test]
    fn observe_does_not_flag_small_output() {
        reset_counters_for_tests();
        observe(Some("Bash"), false, "hello\nworld\n");
        let snap = snapshot_counters();
        assert_eq!(snap.tool_output_oversize_count, 0);
        assert_eq!(snap.total_output_samples, 1);
    }

    #[test]
    fn error_flag_is_propagated_but_does_not_change_threshold() {
        reset_counters_for_tests();
        // An error with tiny content should not flag oversize.
        observe(Some("Bash"), true, "boom\n");
        let snap = snapshot_counters();
        assert_eq!(snap.tool_output_oversize_count, 0);
        assert_eq!(snap.total_output_samples, 1);
    }

    #[test]
    fn unknown_tool_name_is_accepted() {
        reset_counters_for_tests();
        let big = "a".repeat(OVERSIZE_BYTE_THRESHOLD + 16);
        observe(None, false, &big);
        let snap = snapshot_counters();
        assert_eq!(snap.tool_output_oversize_count, 1);
    }
}
