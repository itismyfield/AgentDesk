//! Compatibility access to session-key values and parsers.

pub use crate::domain::session_identity::{SessionIdentity, tmux_name_from_session_key};

impl SessionIdentity {
    /// Render back to the legacy `host:tmux` form.
    #[cfg(test)]
    pub fn legacy_key(&self) -> String {
        format!("{}:{}", self.host, self.tmux_name)
    }
}
