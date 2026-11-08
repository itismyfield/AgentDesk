//! Verified native input attached to its durable birth episode, without cancellation effects.
use super::*;
use crate::services::provider::{
    CancelToken,
    native_stop::{NativeUserPin, PinSaveOutcome},
};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct VerifiedNativeUserPin {
    input: NativeUserPin,
    provider: String,
    channel_id: u64,
    request_owner_user_id: u64,
    user_msg_id: u64,
    turn_nonce: String,
    started_at: String,
}

impl VerifiedNativeUserPin {
    fn new(row: &InflightTurnState, input: NativeUserPin) -> Option<Self> {
        let pin = Self {
            input,
            provider: row.provider.clone(),
            channel_id: row.channel_id,
            request_owner_user_id: row.request_owner_user_id,
            user_msg_id: row.user_msg_id,
            turn_nonce: row.turn_nonce.clone().filter(|n| !n.is_empty())?,
            started_at: row.started_at.clone(),
        };
        // A receipt may precede the runtime handoff; adoption rechecks source coordinates.
        pin.matches_owner(row).then_some(pin)
    }
    fn matches_owner(&self, row: &InflightTurnState) -> bool {
        self.provider == row.provider
            && self.provider == "claude"
            && self.channel_id == row.channel_id
            && self.request_owner_user_id == row.request_owner_user_id
            && self.user_msg_id == row.user_msg_id
            && row.turn_nonce.as_deref() == Some(self.turn_nonce.as_str())
            && self.started_at == row.started_at
            && !row.has_native_herdr_input()
    }
    fn matches_source(&self, row: &InflightTurnState) -> bool {
        self.matches_owner(row)
            && row.runtime_kind
                == Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui)
            && row.output_path.as_deref() == Some(self.input.output_path.as_str())
            && row.tmux_session_name.as_deref() == Some(self.input.tmux_session.as_str())
            && row
                .session_id
                .as_deref()
                .is_none_or(|sid| sid == self.input.session_id)
    }
}

/// Busy inputs the owner handed into this episode's running turn, kept for a recovered token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct NativeBusyChildren {
    provider: String,
    channel_id: u64,
    request_owner_user_id: u64,
    user_msg_id: u64,
    turn_nonce: String,
    started_at: String,
    attempts: Vec<String>,
}

impl NativeBusyChildren {
    fn of(row: &InflightTurnState) -> Option<Self> {
        (row.provider == "claude" && !row.has_native_herdr_input()).then_some(())?;
        Some(Self {
            provider: row.provider.clone(),
            channel_id: row.channel_id,
            request_owner_user_id: row.request_owner_user_id,
            user_msg_id: row.user_msg_id,
            turn_nonce: row.turn_nonce.clone().filter(|n| !n.is_empty())?,
            started_at: row.started_at.clone(),
            attempts: Vec::new(),
        })
    }
    fn owned_by(&self, row: &InflightTurnState) -> bool {
        let episode = Self {
            attempts: Vec::new(),
            ..self.clone()
        };
        Self::of(row).is_some_and(|fresh| fresh == episode)
    }
}

impl InflightTurnState {
    /// Whether this row's own verified input record lies inside `range` of the transcript file
    /// `source` (dev, ino) on `tmux_session`: a frame holding it ends this row's turn, not an older one.
    pub(in crate::services::discord) fn native_input_within(
        &self,
        source: (u64, u64),
        tmux_session: &str,
        range: (u64, u64),
    ) -> bool {
        self.native_stop_user_pin.as_ref().is_some_and(|pin| {
            pin.matches_owner(self)
                && self.tmux_session_name.as_deref() == Some(tmux_session)
                && pin.input.tmux_session == tmux_session
                && pin.input.source_file == source
                && range.0 <= pin.input.range.0
                && pin.input.range.0 < pin.input.range.1
                && pin.input.range.1 <= range.1
        })
    }

    /// Read current proof without granting a missing or foreign child the root's birth.
    pub(in crate::services::discord) fn native_stop_child_proof(
        &self,
        input: &NativeUserPin,
    ) -> Result<(bool, Option<Vec<String>>), &'static str> {
        let root = match &self.native_stop_user_pin {
            Some(pin) if pin.matches_source(self) && pin.input == *input => true,
            // A live receipt may precede RuntimeReady; that root still needs live source proof.
            Some(pin)
                if pin.matches_owner(self)
                    && pin.input == *input
                    && self.runtime_kind
                        == Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui)
                    && self.output_path.is_none()
                    && self.tmux_session_name.as_deref() == Some(input.tmux_session.as_str())
                    && self
                        .session_id
                        .as_deref()
                        .is_none_or(|sid| sid == input.session_id) =>
            {
                false
            }
            Some(_) => return Err("native root birth or source changed"),
            None => false,
        };
        let children = match &self.native_stop_busy_children {
            Some(children) if children.owned_by(self) => Some(children.attempts.clone()),
            Some(_) => return Err("native child belongs to another birth"),
            None => None,
        };
        Ok((root, children))
    }
}

/// A stale same-episode snapshot keeps the verified pin; a successor never inherits it.
pub(super) fn carry_forward(
    existing: Option<&InflightTurnState>,
    outgoing: &mut InflightTurnState,
) {
    if outgoing
        .native_stop_user_pin
        .as_ref()
        .is_some_and(|pin| !pin.matches_owner(outgoing))
    {
        outgoing.native_stop_user_pin = None;
    }
    if let Some(pin) = existing
        .and_then(|row| {
            row.native_stop_user_pin
                .as_ref()
                .filter(|pin| pin.matches_owner(row))
        })
        .filter(|pin| pin.matches_owner(outgoing))
    {
        outgoing.native_stop_user_pin = Some(pin.clone());
    }
    let mut children = (outgoing.native_stop_busy_children.take()).filter(|c| c.owned_by(outgoing));
    if let Some(kept) = existing.and_then(|row| {
        (row.native_stop_busy_children.as_ref()).filter(|c| c.owned_by(row) && c.owned_by(outgoing))
    }) {
        let merged = children.get_or_insert_with(|| kept.clone());
        for attempt in &kept.attempts {
            if !merged.attempts.contains(attempt) {
                merged.attempts.push(attempt.clone());
            }
        }
    }
    outgoing.native_stop_busy_children = children;
}

/// The running turn's token learns a busy attempt before its paste, and the attempt's birth
/// episode keeps it; a failed save leaves a recovered token without it, so its stop stays refused.
pub(in crate::services::discord) fn record_native_busy_child(
    row: &InflightTurnState,
    token: &CancelToken,
    attempt: &str,
) {
    token.native_stop.register_busy_child(attempt);
    let Some(fresh) = NativeBusyChildren::of(row) else {
        return;
    };
    let episode = InflightEpisodePin::from_state(row);
    let claude = &ProviderKind::Claude;
    let Ok(mut locked) =
        episode_guard::lock_inflight_birth_episode(claude, row.channel_id, &episode)
    else {
        return;
    };
    let mut updated = locked.state().clone();
    let mut children = match updated.native_stop_busy_children.take() {
        Some(children) if children.owned_by(&updated) => children,
        _ if fresh.owned_by(&updated) => fresh,
        _ => return,
    };
    if !children.attempts.iter().any(|kept| kept == attempt) {
        children.attempts.push(attempt.to_string());
    }
    updated.native_stop_busy_children = Some(children);
    let _ = locked.persist_verified_under_guard(&updated, "native_busy_child");
}

pub(in crate::services::discord) fn bind_native_stop_pin(
    row: &InflightTurnState,
    token: &Arc<CancelToken>,
) {
    if row.turn_nonce.as_deref().filter(|n| !n.is_empty()) != token.turn_nonce()
        || row.provider != "claude"
        || row.has_native_herdr_input()
    {
        return;
    }
    if let Some(pin) = row
        .native_stop_user_pin
        .as_ref()
        .filter(|pin| pin.matches_source(row))
    {
        token
            .native_stop
            .receipt_sink()
            .restore_candidate(pin.input.clone());
    }
    let children = row
        .native_stop_busy_children
        .iter()
        .filter(|c| c.owned_by(row));
    for attempt in children.flat_map(|children| &children.attempts) {
        token.native_stop.register_busy_child(attempt);
    }
    let episode = InflightEpisodePin::from_state(row);
    let channel = row.channel_id;
    token
        .native_stop
        .receipt_sink()
        .on_pin(Arc::new(move |input| {
            #[cfg(all(test, unix))]
            let _observation = NativePinObservation(channel);
            if input.spawn_nonce.is_none() {
                return PinSaveOutcome::Never;
            }
            let mut locked = match episode_guard::lock_inflight_birth_episode(
                &ProviderKind::Claude,
                channel,
                &episode,
            ) {
                Ok(locked) => locked,
                Err(
                    episode_guard::InflightEpisodeLockError::Missing
                    | episode_guard::InflightEpisodeLockError::Io,
                ) => return PinSaveOutcome::Retry,
                Err(episode_guard::InflightEpisodeLockError::Mismatch) => {
                    return PinSaveOutcome::Never;
                }
            };
            let mut updated = locked.state().clone();
            let Some(pin) = VerifiedNativeUserPin::new(&updated, input.clone()) else {
                return PinSaveOutcome::Never;
            };
            if let Some(existing) = &updated.native_stop_user_pin {
                return if *existing == pin {
                    PinSaveOutcome::Saved
                } else {
                    PinSaveOutcome::Never
                };
            }
            updated.native_stop_user_pin = Some(pin);
            match locked.persist_verified_under_guard(&updated, "native_input_pin") {
                GuardedSaveOutcome::Saved => PinSaveOutcome::Saved,
                GuardedSaveOutcome::IoError => PinSaveOutcome::Retry,
                _ => PinSaveOutcome::Never,
            }
        }));
}

/// Recovery keeps the proof across an app generation as a candidate the stop rechecks.
pub(in crate::services::discord) fn restore_native_stop_pin(
    row: &InflightTurnState,
    token: &Arc<CancelToken>,
) {
    let restored = row
        .native_stop_user_pin
        .as_ref()
        .filter(|pin| pin.matches_source(row));
    if restored.is_some_and(|pin| token.turn_nonce() == Some(pin.turn_nonce.as_str())) {
        bind_native_stop_pin(row, token);
    }
}

#[cfg(all(test, unix))]
static NATIVE_PIN_OBSERVATIONS: (std::sync::Mutex<Vec<(u64, usize)>>, std::sync::Condvar) =
    (std::sync::Mutex::new(Vec::new()), std::sync::Condvar::new());

#[cfg(all(test, unix))]
struct NativePinObservation(u64);

#[cfg(all(test, unix))]
impl Drop for NativePinObservation {
    fn drop(&mut self) {
        let (counts, changed) = &NATIVE_PIN_OBSERVATIONS;
        let mut counts = counts.lock().unwrap_or_else(|error| error.into_inner());
        if let Some((_, count)) = counts.iter_mut().find(|(channel, _)| *channel == self.0) {
            *count += 1;
        } else {
            counts.push((self.0, 1));
        }
        changed.notify_all();
    }
}

#[cfg(all(test, unix))]
pub(in crate::services::discord) fn native_pin_observation_count(channel: u64) -> usize {
    NATIVE_PIN_OBSERVATIONS
        .0
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .find(|(id, _)| *id == channel)
        .map_or(0, |(_, count)| *count)
}

#[cfg(all(test, unix))]
pub(in crate::services::discord) fn wait_native_pin_observations(
    channel: u64,
    expected: usize,
    timeout: std::time::Duration,
) -> bool {
    let (counts, changed) = &NATIVE_PIN_OBSERVATIONS;
    let counts = counts.lock().unwrap_or_else(|error| error.into_inner());
    let (counts, _) = changed
        .wait_timeout_while(counts, timeout, |counts| {
            counts
                .iter()
                .find(|(id, _)| *id == channel)
                .map_or(0, |(_, count)| *count)
                < expected
        })
        .unwrap_or_else(|error| error.into_inner());
    counts
        .iter()
        .find(|(id, _)| *id == channel)
        .is_some_and(|(_, count)| *count >= expected)
}

/// A busy writer's original source and pane, retained until its own native user row appears.
#[cfg(unix)]
pub(in crate::services::discord) struct BusyNativeInput {
    session: String,
    nonce: String,
    frame: String,
    transcript: std::path::PathBuf,
    session_id: String,
    source_file: (u64, u64),
    source_offset: u64,
    pane: crate::services::claude_tui::input::PaneIdentity,
    submitted: Arc<std::sync::atomic::AtomicBool>,
    token: Arc<CancelToken>,
}

#[cfg(unix)]
impl BusyNativeInput {
    pub(in crate::services::discord) fn capture(
        session: &str,
        nonce: &str,
        frame: &str,
        transcript: &std::path::Path,
        source: &serde_json::Value,
        submitted: Arc<std::sync::atomic::AtomicBool>,
        token: Arc<CancelToken>,
    ) -> Option<Self> {
        if source["kind"] != "existing"
            || source["transcript"].as_str()? != transcript.to_str()?
            || session.is_empty()
            || nonce.is_empty()
        {
            return None;
        }
        let session_id = source["session_id"].as_str()?.to_owned();
        if session_id.is_empty() {
            return None;
        }
        Some(Self {
            session: session.into(),
            nonce: nonce.into(),
            frame: frame.into(),
            transcript: transcript.into(),
            session_id,
            source_file: (source["file"][0].as_u64()?, source["file"][1].as_u64()?),
            source_offset: source["offset"].as_u64()?,
            pane: crate::services::claude_tui::input::PaneIdentity::capture(session)?,
            submitted,
            token,
        })
    }

    pub(in crate::services::discord) fn adopt(&self, offset: u64) {
        if self.submitted.load(std::sync::atomic::Ordering::Acquire)
            && let Some(pin) = self.verify(offset)
        {
            self.token.native_stop.receipt_sink().adopt(pin);
        }
    }

    fn verify(&self, offset: u64) -> Option<NativeUserPin> {
        use crate::services::claude_tui::input::{PaneIdentity, UserReceipt};
        use crate::services::tui_input::human_frame::history::{self, RowOwner};
        use history::native_row::{native_prompt, read_native_row};

        const MAX_ROW: u64 = 128 * 1024;
        if offset < self.source_offset || PaneIdentity::capture(&self.session)? != self.pane {
            return None;
        }
        let native = read_native_row(
            &self.transcript,
            [self.source_file.0, self.source_file.1],
            offset,
            MAX_ROW,
            false,
        )?;
        let record = &native.record;
        if record["type"] != "user" || native_prompt(record, &self.session_id).is_none() {
            return None;
        }
        let end = native.range.1;
        let mut pin = UserReceipt::native_pin_before_tail(
            &self.session,
            &self.frame,
            Some(&self.session_id),
            end,
        )?;
        if pin.uuid != record["uuid"].as_str()?
            || pin.range != native.range
            || pin.row_digest != native.digest
            || pin.source_file != (native.file[0], native.file[1])
            || pin.source_file != self.source_file
            || pin.session_id != self.session_id
            || std::path::Path::new(&pin.output_path) != self.transcript
            || pin.tmux_session != self.session
            || PaneIdentity::capture(&self.session)? != self.pane
            || !matches!(
                history::row_owner("claude", &self.session, std::slice::from_ref(&self.frame), &pin.uuid),
                RowOwner::User(nonce) if nonce == self.nonce
            )
        {
            return None;
        }
        pin.direct = false;
        Some(pin)
    }
}

#[cfg(all(test, unix))]
#[path = "native_stop_pin/busy_input_tests.rs"]
mod busy_input_tests;

/// Verifies the persisted input in its historical transcript without consulting the current pane.
pub(in crate::services::discord) fn historical_input_verified(row: &InflightTurnState) -> bool {
    let Some(pin) = row
        .native_stop_user_pin
        .as_ref()
        .filter(|pin| pin.matches_owner(row))
    else {
        return false;
    };
    #[cfg(not(unix))]
    {
        let _ = pin;
        false
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
        let verify = || -> Option<()> {
            let input = &pin.input;
            let length = input.range.1.checked_sub(input.range.0)?;
            if input.uuid.trim().is_empty()
                || input.session_id.trim().is_empty()
                || pin.turn_nonce.trim().is_empty()
                || input
                    .spawn_nonce
                    .as_deref()
                    .is_none_or(|nonce| nonce.trim().is_empty())
                || input.range.0 < input.receipt_start
                || length == 0
                || length > 16 * 1024 * 1024
            {
                return None;
            }
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&input.output_path)
                .ok()?;
            let before = file.metadata().ok()?;
            if !before.is_file()
                || before.ino() != input.source_file.1
                || before.len() < input.range.1
            {
                return None;
            }
            let mut raw = vec![0; usize::try_from(length).ok()?];
            file.read_exact_at(&mut raw, input.range.0).ok()?;
            if !raw.ends_with(b"\n") || blake3::hash(&raw).to_hex().as_str() != input.row_digest {
                return None;
            }
            let record: serde_json::Value = serde_json::from_slice(&raw).ok()?;
            let (_, uuid) = crate::services::tui_prompt_dedupe::extract_claude_transcript_user_prompt_with_entry_id(&record)?;
            if uuid.as_deref() != Some(input.uuid.as_str())
                || record["sessionId"].as_str() != Some(input.session_id.as_str())
                || record["isSidechain"] == true
            {
                return None;
            }
            let current = std::fs::symlink_metadata(&input.output_path).ok()?;
            (current.is_file()
                && current.dev() == before.dev()
                && current.ino() == before.ino()
                && current.len() >= input.range.1)
                .then_some(())
        };
        verify().is_some()
    }
}

#[cfg(all(test, unix))]
#[path = "native_stop_pin/historical_tests.rs"]
mod historical_tests;
