//! `agentdesk herdr probe-user-default`: one read-only next-restart check of the execution user's
//! own herdr server, before any AgentDesk config, database or server is involved.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

/// Prints the same reason/action JSON health shows; exit 0 means the check ran, not that it passed.
pub(super) fn run(
    socket: Option<PathBuf>,
    home: Option<OsString>,
    out: &mut dyn Write,
) -> Result<(), String> {
    let socket = match socket {
        Some(socket) => socket,
        // The socket a local user_default endpoint resolves when its socket_path is omitted.
        None => PathBuf::from(home.ok_or("HOME is unset; pass --socket")?)
            .join(".config/herdr/herdr.sock"),
    };
    if !socket.is_absolute() {
        return Err(format!(
            "herdr probe: socket {} is not absolute",
            socket.display()
        ));
    }
    super::super::direct::run_async(async move {
        let report = crate::services::session_host::policy::diagnostics::probe(&socket).await?;
        let text = serde_json::to_string_pretty(&report).unwrap_or_else(|_| report.to_string());
        writeln!(out, "{text}").map_err(|error| format!("herdr probe output: {error}"))
    })
}

#[cfg(test)]
#[path = "herdr_probe_tests.rs"]
mod tests;
