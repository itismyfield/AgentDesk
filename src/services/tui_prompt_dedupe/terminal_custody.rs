//! Terminal delivery custody reads for callers below Discord, through readers the runtime installs.

use super::clear_relaunch::Captured;
use std::sync::OnceLock;

/// The custody store's two read-only questions, supplied by `discord::terminal_delivery_custody`.
pub(crate) struct TerminalCustody {
    pub(crate) retains_channel: fn(u64) -> Result<bool, String>,
    pub(crate) codex_absent_launch_clear: fn(&Captured) -> bool,
}

static READERS: OnceLock<TerminalCustody> = OnceLock::new();

/// Installs the custody store's readers beside the health registry; the first install wins.
pub(crate) fn install(readers: TerminalCustody) {
    let _ = READERS.set(readers);
}

fn readers() -> Option<&'static TerminalCustody> {
    #[cfg(test)]
    crate::services::discord::terminal_delivery_custody::install_runtime_reads();
    READERS.get()
}

/// Whether a retained episode may belong to `channel`; an uninstalled reader reads as an error.
pub(crate) fn retains_channel(channel: u64) -> Result<bool, String> {
    let readers = readers().ok_or("terminal delivery custody reader not installed")?;
    (readers.retains_channel)(channel)
}

/// Whether no pending custody holds this capture; an uninstalled reader never reads clear.
pub(crate) fn codex_absent_launch_clear(captured: &Captured) -> bool {
    readers().is_some_and(|readers| (readers.codex_absent_launch_clear)(captured))
}
