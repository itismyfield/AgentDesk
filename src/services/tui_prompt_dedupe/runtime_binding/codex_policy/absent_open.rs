//! Absence opens only a fresh launch; existing source and clear permissions stay separate.

use super::{Captured, capture_under as capture_pin};
use crate::services::codex_tui::canary::CANARY_CHANNEL;
use crate::services::discord::codex_absence::InflightObs;
use crate::services::platform::tmux::{self, PaneLiveness, SessionPresence};
use crate::services::tmux_common::{self as tc, TmuxSourceAuthority};
use crate::services::tui_prompt_dedupe::{
    binding_context::{self, BindingContext, SpawnNonceMarker},
    binding_events,
};
use std::{fs, io, path::PathBuf};

pub(super) fn capture_under(
    authority: &TmuxSourceAuthority<'_>,
    observation: &InflightObs,
) -> Option<Captured> {
    let captured = candidate_under(authority)?;
    (observation.recheck(CANARY_CHANNEL, authority.session())
        && crate::services::discord::terminal_delivery_custody::codex_absent_launch_clear(
            &captured,
        ))
    .then_some(captured)
}

pub(super) fn custody_pending_under(authority: &TmuxSourceAuthority<'_>) -> bool {
    candidate_under(authority).is_some_and(|captured| {
        !crate::services::discord::terminal_delivery_custody::codex_absent_launch_clear(&captured)
    })
}

pub(super) fn candidate_under(authority: &TmuxSourceAuthority<'_>) -> Option<Captured> {
    let captured = capture_pin(authority).ok()??;
    let tmux = authority.session();
    if !scoped_context(&captured.context, tmux) || !target_absent(tmux) {
        return None;
    }
    let nonce = read_nonce(tmux);
    let known = match &nonce {
        SpawnNonceMarker::Absent => false,
        SpawnNonceMarker::Known(nonce) if nonce == &captured.nonce => {
            if binding_context::execution_context("codex", nonce)
                .ok()
                .as_ref()
                != Some(&captured.context)
            {
                return None;
            }
            true
        }
        _ => return None,
    };
    for path in marker_paths(tmux, tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT)? {
        let Some(value) = read_json(&path).ok()? else {
            continue;
        };
        if !rollout_ownership_matches(&value, &captured.nonce, known) {
            return None;
        }
    }
    Some(captured)
}

pub(super) fn scoped_context(context: &BindingContext, tmux: &str) -> bool {
    use crate::services::provider_hosting::{
        ProviderSessionDriver, resolve_provider_session_selection_with_channel,
    };
    local_context(context, tmux)
        && !crate::services::herdr_launch::herdr_configured_for_tui_launch(Some(CANARY_CHANNEL))
        && runtime_markers_match(tmux)
        && resolve_provider_session_selection_with_channel(
            &crate::services::provider::ProviderKind::Codex,
            true,
            Some(CANARY_CHANNEL),
        )
        .driver
            == ProviderSessionDriver::TuiHosting
}

fn local_context(context: &BindingContext, tmux: &str) -> bool {
    context.provider == "codex"
        && context.tmux_session == tmux
        && context.channel_id == Some(CANARY_CHANNEL)
        && context.source_policy.as_deref() == Some("verified")
        && context.owner_runtime_root == tc::current_tmux_owner_marker()
        && context
            .host
            .as_deref()
            .is_some_and(|host| !host.trim().is_empty())
        && context.host == binding_context::stable_host_identity()
}

fn target_absent(tmux: &str) -> bool {
    tmux::session_presence(tmux) == SessionPresence::Missing
        && crate::services::session_backend::process_session_pid(tmux).is_none()
        && tmux::pane_liveness(tmux) == PaneLiveness::DeadOrAbsent
}

fn runtime_markers_match(tmux: &str) -> bool {
    [
        (tc::TMUX_RUNTIME_KIND_TEMP_EXT, "codex_tui"),
        ("host_kind", "tmux"),
    ]
    .into_iter()
    .all(|(extension, expected)| {
        marker_paths(tmux, extension).is_some_and(|paths| {
            paths
                .into_iter()
                .all(|path| match fs::read_to_string(path) {
                    Ok(value) => value.trim() == expected,
                    Err(error) => error.kind() == io::ErrorKind::NotFound,
                })
        })
    })
}

// The legacy path builder is pure; reusing its basename avoids the canonical mkdir helper.
fn marker_paths(tmux: &str, extension: &str) -> Option<[PathBuf; 2]> {
    let legacy = PathBuf::from(tc::legacy_tmp_session_path(tmux, extension));
    let canonical = tc::persistent_sessions_dir()?.join(legacy.file_name()?);
    Some([canonical, legacy])
}

pub(super) fn read_nonce(tmux: &str) -> SpawnNonceMarker {
    let Some(paths) = marker_paths(tmux, "spawn_nonce") else {
        return SpawnNonceMarker::Unreadable;
    };
    read_nonce_paths(paths)
}

fn read_nonce_paths(paths: [PathBuf; 2]) -> SpawnNonceMarker {
    let mut observed = None;
    for path in paths {
        match fs::read_to_string(path) {
            Ok(value) if !value.trim().is_empty() => {
                let nonce = value.trim().to_owned();
                if observed.as_ref().is_some_and(|previous| previous != &nonce) {
                    return SpawnNonceMarker::Unreadable;
                }
                observed = Some(nonce);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => return SpawnNonceMarker::Unreadable,
        }
    }
    observed.map_or(SpawnNonceMarker::Absent, SpawnNonceMarker::Known)
}

fn read_json(path: &std::path::Path) -> Result<Option<serde_json::Value>, io::Error> {
    use io::Read;
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized launch marker",
        ));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(io::Error::other)
}

fn rollout_ownership_matches(value: &serde_json::Value, nonce: &str, known: bool) -> bool {
    let Some(marker) = value.as_object() else {
        return false;
    };
    let Some(ownership) = marker.get("codex_ownership") else {
        return true;
    };
    let Some(ownership) = ownership.as_object() else {
        return false;
    };
    known
        && ownership
            .get("execution_nonce")
            .and_then(serde_json::Value::as_str)
            == Some(nonce)
        && ["proof_seq", "dev", "ino"].into_iter().all(|field| {
            ownership
                .get(field)
                .and_then(serde_json::Value::as_u64)
                .is_some()
        })
}

pub(super) fn successor_unproved_under(
    authority: &TmuxSourceAuthority<'_>,
    nonce: &str,
    context: &BindingContext,
) -> bool {
    let Some(captured) = capture_pin(authority).ok().flatten() else {
        return false;
    };
    nonce != captured.nonce
        && context.execution_nonce == nonce
        && local_context(context, authority.session())
        && context.created_at > captured.committed_at
        && target_absent(authority.session())
        && binding_events::codex::context_for_nonce(
            CANARY_CHANNEL,
            authority.session(),
            Some(nonce),
        )
        .is_ok_and(|context| context.is_none())
}

#[cfg(test)]
#[path = "absent_open_tests.rs"]
mod tests;
