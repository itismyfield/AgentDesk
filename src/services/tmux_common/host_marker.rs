//! `.host_kind` session marker. The marker is location evidence, not recovery
//! authority; the Claude TUI launch writes it for its tmux session.
#![cfg_attr(not(test), allow(dead_code))]

use super::session_temp_path;
use crate::services::session_host::HostKind;

const HOST_KIND_TEMP_EXT: &str = "host_kind";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostKindMarker {
    Absent,
    Known(HostKind),
    /// Present but empty, truncated or naming a host this binary does not know.
    Unrecognized(String),
    /// Present but unreadable; never reported as absent.
    ReadFailed(String),
}

/// Reads only the canonical path: the pre-migration `/tmp` location never held this marker.
pub(crate) fn read_host_kind_marker(session_name: &str) -> HostKindMarker {
    let path = session_temp_path(session_name, HOST_KIND_TEMP_EXT);
    match std::fs::read_to_string(&path) {
        Ok(raw) => match HostKind::from_persisted(raw.trim()) {
            Some(kind) => HostKindMarker::Known(kind),
            None => HostKindMarker::Unrecognized(raw),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => HostKindMarker::Absent,
        Err(error) => HostKindMarker::ReadFailed(format!("{path}: {error}")),
    }
}

/// Marks a tmux-hosted session before its launch. A failed write only warns: the
/// launch proceeds as before and the marker reads as absent or unrecognized.
pub(crate) fn record_tmux_host_marker(session_name: &str) {
    let path = session_temp_path(session_name, HOST_KIND_TEMP_EXT);
    if let Err(error) = std::fs::write(&path, HostKind::Tmux.as_str()) {
        tracing::warn!(session_name, %path, %error, "host kind marker write failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker_path(session: &str) -> String {
        session_temp_path(session, HOST_KIND_TEMP_EXT)
    }

    #[test]
    fn host_kind_marker_reports_every_on_disk_state_without_a_tmux_default() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let session = "AgentDesk-claude-host-marker";
        assert_eq!(read_host_kind_marker(session), HostKindMarker::Absent);

        for (written, expected) in [
            ("tmux", HostKindMarker::Known(HostKind::Tmux)),
            ("herdr\n", HostKindMarker::Known(HostKind::Herdr)),
            ("process", HostKindMarker::Known(HostKind::Process)),
            ("zellij", HostKindMarker::Unrecognized("zellij".to_string())),
            ("Herdr", HostKindMarker::Unrecognized("Herdr".to_string())),
            ("herd", HostKindMarker::Unrecognized("herd".to_string())),
            ("", HostKindMarker::Unrecognized(String::new())),
        ] {
            std::fs::write(marker_path(session), written).unwrap();
            assert_eq!(read_host_kind_marker(session), expected, "{written:?}");
        }

        std::fs::write(marker_path(session), [0xff, 0xfe]).unwrap();
        assert!(matches!(
            read_host_kind_marker(session),
            HostKindMarker::ReadFailed(_)
        ));
        std::fs::remove_file(marker_path(session)).unwrap();
        std::fs::create_dir(marker_path(session)).unwrap();
        assert!(
            matches!(
                read_host_kind_marker(session),
                HostKindMarker::ReadFailed(_)
            ),
            "an unreadable marker must not read as absent"
        );
    }
}
