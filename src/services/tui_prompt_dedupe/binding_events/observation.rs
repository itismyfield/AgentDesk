use super::*;

impl Writer {
    pub(super) fn claude_pane_history(
        &self,
        session: &str,
        nonce: Option<&str>,
    ) -> (Option<PinnedAt>, SourceHistory) {
        let pane = self.panes.get(session);
        let pin = pane
            .filter(|pane| pane.verified)
            .and_then(|pane| Some((pane.current.clone()?, pane.pinned_at)));
        let history = match pane {
            Some(pane) => pane.claude.history(nonce, self.tainted),
            None => claude_fold::ClaudeFold::default().history(nonce, self.tainted),
        };
        (pin, history)
    }
}

#[cfg(test)]
pub(crate) fn published_claude_history(
    channel: u64,
    session: &str,
    nonce: &str,
) -> Option<(Option<PinnedAt>, SourceHistory)> {
    let path = log_path(channel).ok()??;
    let logs = lock_logs();
    let writer = logs
        .get(&path)?
        .writer
        .as_ref()
        .filter(|writer| !writer.poisoned)?;
    Some(writer.claude_pane_history(session, Some(nonce)))
}
