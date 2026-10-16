use super::super::super::inflight::{InflightEpisodePin, InflightTurnState};
use super::super::super::tui_direct_abort_marker as markers;
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::channel_policy::{Adoption::Committed, Candidate};
use crate::services::tui_o::shadow::tap::TuiOConfig;
use crate::services::tui_o::turn_mode::TurnConfig;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::services::discord) struct Retirement {
    pub removed: usize,
    pub retry_pending: bool,
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn records<T: serde::de::DeserializeOwned>(
    root: Option<PathBuf>,
    provider: &ProviderKind,
    channel: u64,
) -> Result<Vec<(PathBuf, T)>, String> {
    let Some(root) = root else {
        return Ok(Vec::new());
    };
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", root.display())),
    };
    let prefix = format!("{}_{}_", provider.as_str(), channel);
    let mut out = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_some_and(|e| e == "json")
            && path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
            && let Some(bytes) = read_optional(&path)?
        {
            let record =
                serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
            out.push((path, record));
        }
    }
    Ok(out)
}

// Read the entire population before unlinking; a read failure leaves Legacy intact.
// Removal retires synthetic ownership and supplies no delivery-completion evidence.
pub(in crate::services::discord) fn retire_channel(
    provider: &ProviderKind,
    channel: u64,
) -> Result<Retirement, String> {
    let row_path = inflight::inflight_runtime_root()
        .map(|root| inflight::inflight_state_path(&root, provider, channel));
    let row = row_path
        .as_deref()
        .map(read_optional)
        .transpose()?
        .flatten()
        .map(|bytes| {
            serde_json::from_slice::<InflightTurnState>(&bytes)
                .map(|state| (bytes, state))
                .map_err(|e| e.to_string())
        })
        .transpose()?;
    let pending: Vec<(PathBuf, TuiDirectPendingStart)> = records(root(), provider, channel)?;
    let markers: Vec<(PathBuf, markers::AbortedAnchorMarker)> = records(
        runtime_store::tui_direct_abort_marker_root(),
        provider,
        channel,
    )?;
    if pending.iter().any(|(path, r)| {
        r.provider != provider.as_str()
            || r.channel_id != channel
            || path
                .file_stem()
                .is_none_or(|stem| stem != r.file_stem().as_str())
    }) || markers.iter().any(|(path, r)| {
        r.provider != provider.as_str()
            || r.channel_id != channel
            || path.file_stem().is_none_or(|stem| {
                stem != format!("{}_{}_{}", r.provider, r.channel_id, r.anchor_message_id).as_str()
            })
    }) || row
        .as_ref()
        .is_some_and(|(_, r)| r.provider != provider.as_str() || r.channel_id != channel)
    {
        return Err("retirement population does not match its channel".into());
    }
    #[cfg(test)]
    pause_before_removal(channel);
    let mut result = Retirement::default();
    if let Some((bytes, state)) = row
        && inflight::is_synthetic_create_state(&state)
        && let Some(path) = row_path
    {
        let outcome = inflight::lock_inflight_state_path(&path).ok().map(|guard| {
            if std::fs::read(&path).ok().as_deref() != Some(&bytes) {
                return inflight::GuardedClearOutcome::UserMsgMismatch;
            }
            inflight::operator_disposition_remove_pinned(
                &guard,
                &InflightEpisodePin::from_state(&state),
            )
            .0
        });
        match outcome {
            Some(inflight::GuardedClearOutcome::Cleared) => result.removed += 1,
            Some(inflight::GuardedClearOutcome::Missing) => {}
            _ => result.retry_pending = true,
        }
    }
    {
        let _guard = PRESENCE_RECONCILE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (path, record) in pending {
            match read_optional(&path) {
                Ok(None) => {}
                Ok(Some(bytes))
                    if serde_json::from_slice::<TuiDirectPendingStart>(&bytes)
                        .ok()
                        .as_ref()
                        == Some(&record) =>
                {
                    delete_locked(&record);
                    if matches!(read_optional(&path), Ok(None)) {
                        result.removed += 1;
                    } else {
                        result.retry_pending = true;
                    }
                }
                _ => result.retry_pending = true,
            }
        }
    }
    for (path, marker) in markers {
        let Some(_claim) = markers::try_claim_marker(&marker) else {
            result.retry_pending = true;
            continue;
        };
        match read_optional(&path) {
            Ok(None) => {}
            Ok(Some(bytes))
                if serde_json::from_slice::<markers::AbortedAnchorMarker>(&bytes)
                    .ok()
                    .as_ref()
                    == Some(&marker) =>
            {
                markers::delete(&marker);
                if matches!(read_optional(&path), Ok(None)) {
                    result.removed += 1;
                } else {
                    result.retry_pending = true;
                }
            }
            _ => result.retry_pending = true,
        }
    }
    Ok(result)
}

/// Confirms each selected channel of `owned` once its retirement left nothing behind; a failed or
/// partial retirement keeps the channel on Legacy turns for this process.
pub(in crate::services::discord) fn confirm_turn_channels(
    provider: &ProviderKind,
    config: Option<&TurnConfig>,
    owned: impl FnOnce() -> Vec<u64>,
) -> Vec<u64> {
    let retire = |channel| match retire_channel(provider, channel) {
        Ok(Retirement {
            removed,
            retry_pending: false,
        }) => {
            tracing::info!(
                channel,
                removed,
                "[tui_o] turn mode confirmed after retirement"
            );
            true
        }
        outcome => {
            tracing::error!(
                channel,
                ?outcome,
                "[tui_o] turn mode refused; Legacy keeps turns"
            );
            false
        }
    };
    crate::services::tui_o::turn_mode::confirm_selected(config, owned, retire)
}

/// Boot confirmation over the channels whose O adoption this provider's boot policy committed.
pub(in crate::services::discord) fn confirm_at_boot(
    provider: &ProviderKind,
    config: Option<&TuiOConfig>,
) -> Vec<u64> {
    confirm_turn_channels(provider, config.map(|c| &c.turn), || {
        let kind = match provider {
            ProviderKind::Claude => RuntimeHandoffKind::ClaudeTui,
            ProviderKind::Codex => RuntimeHandoffKind::CodexTui,
            _ => return Vec::new(),
        };
        let committed = |c: &Option<Candidate>| c.as_ref().is_some_and(|c| c.peek() == Committed);
        crate::services::tui_o::cutover::boot_ownership()
            .into_iter()
            .filter(|(_, k, candidate)| *k == Some(kind) && committed(candidate))
            .map(|(channel, _, _)| channel)
            .collect()
    })
}

#[cfg(test)]
static PAUSE: Mutex<
    Option<(
        u64,
        std::sync::mpsc::Sender<()>,
        std::sync::mpsc::Receiver<()>,
    )>,
> = Mutex::new(None);

#[cfg(test)]
fn pause_before_removal(channel: u64) {
    let pause = {
        let mut guard = PAUSE.lock().unwrap_or_else(|e| e.into_inner());
        if guard.as_ref().is_some_and(|p| p.0 == channel) {
            guard.take()
        } else {
            None
        }
    };
    if let Some((_, reached, resume)) = pause {
        reached.send(()).unwrap();
        resume
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();
    }
}

#[cfg(test)]
#[path = "turn_retirement_tests.rs"]
mod tests;
