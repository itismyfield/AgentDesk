//! Observe the loaded card state before sync applies its database effects.

use super::{GhIssue, PgCardRecord};

pub(super) fn observe(
    repo: &str,
    issue: &GhIssue,
    card: &PgCardRecord,
    pipeline: &crate::pipeline::PipelineConfig,
) -> bool {
    let is_terminal = pipeline.is_terminal(&card.status);
    crate::github::warn_dedupe::terminal_open(
        repo,
        issue.number,
        &card.id,
        issue.state == "OPEN" && is_terminal,
    );
    is_terminal
}
