//! A confirmed clear retires history for launching, without granting source consumption.

use super::*;
use crate::services::codex_tui::canary::{CANARY_CHANNEL, CANARY_TMUX};
use crate::services::platform::tmux::{self, SessionPresence};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{fs, io::Read, path::PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Captured {
    pub seq: u64,
    pub nonce: String,
    pub committed_at: DateTime<Utc>,
    pub context: BindingContext,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Watermark {
    schema: u32,
    channel: u64,
    tmux: String,
    owner_runtime_root: String,
    retired_through_seq: u64,
    nonce: String,
    origin: String,
    at: DateTime<Utc>,
}

fn path() -> Result<PathBuf, String> {
    crate::config::runtime_root()
        .map(|root| {
            root.join("runtime/codex_clear_retired")
                .join(format!("{CANARY_CHANNEL}.json"))
        })
        .ok_or_else(|| "retirement root unavailable".to_owned())
}

fn read() -> Result<Option<Watermark>, String> {
    let file = match fs::File::open(path()?) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let mut bytes = Vec::new();
    file.take(1025)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 1024 {
        return Err("retirement exceeds limit".into());
    }
    let mark: Watermark = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if mark.schema != 1
        || mark.channel != CANARY_CHANNEL
        || mark.tmux != CANARY_TMUX
        || mark.owner_runtime_root != tc::current_tmux_owner_marker()
        || mark.retired_through_seq == 0
        || mark.nonce.len() != 32
        || !mark.nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !matches!(
            mark.origin.as_str(),
            "explicit_clear" | "legacy_clear_boundary"
        )
    {
        return Err("retirement identity mismatch".into());
    }
    Ok(Some(mark))
}

fn capture_under(authority: &TmuxSourceAuthority<'_>) -> Result<Option<Captured>, String> {
    let tmux = authority.session();
    if tmux != CANARY_TMUX {
        return Ok(None);
    }
    let Some((seq, nonce, committed_at)) =
        binding_events::codex::latest_ownership(CANARY_CHANNEL, tmux).map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let context = binding_events::codex::context_for_nonce(CANARY_CHANNEL, tmux, Some(&nonce))
        .map_err(|e| e.to_string())?
        .ok_or("ownership context unavailable")?;
    if context.owner_runtime_root != tc::current_tmux_owner_marker()
        || context.host != binding_context::stable_host_identity()
    {
        return Err("ownership runtime mismatch".into());
    }
    // Only a verified pin holds launches, so any other incarnation has nothing to retire.
    if context.source_policy.as_deref() != Some("verified") {
        return Ok(None);
    }
    Ok(Some(Captured {
        seq,
        nonce,
        committed_at,
        context,
    }))
}

pub(crate) fn capture(tmux: &str) -> Result<Option<Captured>, String> {
    tc::with_tmux_source_authority(tmux, capture_under)
}

fn dead(tmux: &str) -> bool {
    tmux::session_presence(tmux) == SessionPresence::Missing
        && crate::services::session_backend::process_session_pid(tmux).is_none()
}

fn vacant_under(authority: &TmuxSourceAuthority<'_>) -> bool {
    let tmux = authority.session();
    if binding_context::observe_spawn_nonce_marker(tmux) != SpawnNonceMarker::Absent {
        return false;
    }
    let marker = tc::session_temp_path(tmux, tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT);
    let unowned = match fs::read(marker) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .is_ok_and(|value| value.get("codex_ownership").is_none()),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    };
    unowned && dead(tmux)
}

pub(crate) fn vacant(tmux: &str) -> bool {
    tc::with_tmux_source_authority(tmux, vacant_under)
}

pub(super) fn open_under(authority: &TmuxSourceAuthority<'_>) -> bool {
    if authority.session() != CANARY_TMUX || !vacant_under(authority) {
        return false;
    }
    let (Ok(Some(current)), Ok(Some(mark))) = (capture_under(authority), read()) else {
        return false;
    };
    current.seq <= mark.retired_through_seq && current.nonce == mark.nonce
}

pub(crate) fn relaunch_open(tmux: &str) -> bool {
    tc::with_tmux_source_authority(tmux, open_under)
}

pub(crate) fn launch_blocked(tmux: &str) -> bool {
    codex_verified_input_blocked(tmux) && !relaunch_open(tmux)
}

pub(crate) fn commit(captured: &Captured, origin: &str) -> Result<(), String> {
    tc::with_tmux_source_authority(&captured.context.tmux_session, |authority| {
        if capture_under(authority)?.as_ref() != Some(captured) {
            return Err("ownership changed after clear capture".into());
        }
        // A missing pane can leave launch files behind when its kill and nonce stamp raced.
        if origin == "explicit_clear"
            && dead(authority.session())
            && let SpawnNonceMarker::Known(nonce) =
                binding_context::observe_spawn_nonce_marker(authority.session())
            && binding_context::execution_context("codex", &nonce).is_ok_and(|context| {
                context.tmux_session == authority.session()
                    && context.owner_runtime_root == captured.context.owner_runtime_root
            })
        {
            tc::cleanup_session_temp_files_under_source_authority(authority, authority.session());
        }
        if !vacant_under(authority) {
            return Err("clear target is not confirmed vacant".into());
        }
        if origin != "explicit_clear" {
            return Err("unknown retirement origin".into());
        }
        let mark = Watermark {
            schema: 1,
            channel: CANARY_CHANNEL,
            tmux: CANARY_TMUX.into(),
            owner_runtime_root: captured.context.owner_runtime_root.clone(),
            retired_through_seq: captured.seq,
            nonce: captured.nonce.clone(),
            origin: origin.into(),
            at: Utc::now(),
        };
        let bytes = serde_json::to_string(&mark).map_err(|e| e.to_string())?;
        if bytes.len() > 1024 {
            return Err("retirement exceeds limit".into());
        }
        crate::services::discord::runtime_store::atomic_write(&path()?, &bytes)
    })
}
