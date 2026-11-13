use crate::services::session_host::HostKey;
use std::process::Output;

/// Pane operations used by the hosted input adapters.
pub(crate) trait InputTransport {
    fn send_literal(&mut self, session: &str, text: &str) -> Result<Output, String>;
    fn load_buffer(&mut self, buffer: &str, text: &str) -> Result<Output, String>;
    fn paste_buffer(&mut self, session: &str, buffer: &str, delete: bool)
    -> Result<Output, String>;
    fn send_keys(&mut self, session: &str, keys: &[HostKey]) -> Result<Output, String>;
    fn capture(&mut self, session: &str, scroll_back: i32) -> Option<String>;
    /// The pane for the draft-protection reader; a transport that cannot keep its attributes
    /// returns nothing, which keeps a protected pane held.
    fn capture_draft(&mut self, _session: &str) -> Option<String> {
        None
    }
    fn pane_alive(&mut self, session: &str) -> bool;
    fn present(&mut self, session: &str) -> bool;
    /// Records the termination and exit reason, then kills the session.
    fn retire(&mut self, session: &str, reason_code: &str, reason: &str);
}
