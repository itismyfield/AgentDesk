//! Historical terminal sequences close observations without granting source ownership.

use super::*;

pub(crate) fn terminal_pending_at_seq(channel: u64, seq: u64) -> io::Result<Option<u64>> {
    let _logs = lock_logs();
    Ok(ownership_records(channel)?.into_iter().find_map(|logged| {
        if logged.event.seq != seq
            || logged.event.provider != "codex"
            || !matches!(logged.event.new, BindingTarget::Rejected { .. })
        {
            return None;
        }
        logged.codex_ownership?.pending_seq
    }))
}
