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
