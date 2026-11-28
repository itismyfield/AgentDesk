use super::{HostCapabilities, HostKind, TmuxHost};
use crate::services::tmux_common::{TmuxSourceAuthority, with_tmux_source_authority};

pub type IdleCleanupPreserved = (&'static str, Option<serde_json::Value>);
type ProveIdle<'a> =
    Box<dyn FnOnce(&TmuxSourceAuthority<'_>) -> Result<(), IdleCleanupPreserved> + 'a>;

pub struct IdleCleanupRequest<'a> {
    pub session_name: &'a str,
    pub reason: &'a str,
    pub prove_idle: ProveIdle<'a>,
}

pub(super) fn support(kind: HostKind, capabilities: HostCapabilities) -> Result<(), &'static str> {
    match kind {
        HostKind::Tmux if capabilities.idle_cleanup => Ok(()),
        HostKind::Herdr => Err("herdr_idle_cleanup_requires_core"),
        _ => Err("host_idle_cleanup_unsupported"),
    }
}

pub(super) fn cleanup(
    kind: HostKind,
    request: IdleCleanupRequest<'_>,
) -> Result<bool, IdleCleanupPreserved> {
    if kind != HostKind::Tmux {
        return Err(("host_idle_cleanup_unsupported", None));
    }
    with_tmux_source_authority(request.session_name, |authority| {
        (request.prove_idle)(authority)?;
        Ok(TmuxHost
            .kill_output_timeout(
                request.session_name,
                request.reason,
                std::time::Duration::from_secs(2),
            )
            .is_ok_and(|output| output.status.success()))
    })
}
