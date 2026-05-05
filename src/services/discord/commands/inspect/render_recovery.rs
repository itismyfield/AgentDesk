use super::formatting::{
    fenced_report, format_kst, human_recovery_source, opt_or_none, push_kv, push_line,
    truncate_chars,
};
use crate::services::observability::recovery_audit::RecoveryAuditRecord;

const RECOVERY_PREVIEW_MAX_LINES: usize = 8;
const RECOVERY_PREVIEW_LINE_MAX_CHARS: usize = 92;

pub(super) fn render_recovery_report(record: &RecoveryAuditRecord) -> String {
    let mut out = String::new();
    push_line(&mut out, "Recovery Context");
    push_kv(&mut out, "source", &human_recovery_source(&record.source));
    push_kv(&mut out, "created", &format_kst(record.created_at));
    push_kv(&mut out, "messages", &record.message_count.to_string());
    push_kv(
        &mut out,
        "max chars/message",
        &record.max_chars_per_message.to_string(),
    );
    push_kv(
        &mut out,
        "authors",
        &truncate_chars(&record.authors.join(", "), 76),
    );
    push_kv(
        &mut out,
        "consumed_by_turn",
        opt_or_none(record.consumed_by_turn_id.as_deref()),
    );
    push_kv(
        &mut out,
        "sha256",
        &truncate_chars(&record.content_sha256, 16),
    );
    push_line(&mut out, "");
    push_line(&mut out, "Preview (redacted):");
    let mut wrote_preview = false;
    for (idx, line) in record
        .redacted_preview
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(RECOVERY_PREVIEW_MAX_LINES)
        .enumerate()
    {
        wrote_preview = true;
        push_line(
            &mut out,
            &format!(
                "{}. {}",
                idx + 1,
                truncate_chars(line.trim(), RECOVERY_PREVIEW_LINE_MAX_CHARS)
            ),
        );
    }
    if !wrote_preview {
        push_line(&mut out, "(redacted preview 없음)");
    }
    fenced_report(out)
}
