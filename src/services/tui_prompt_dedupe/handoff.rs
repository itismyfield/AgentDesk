//! A Discord turn reserves its Codex handoff before submit; an input seen before the handoff
//! names its native turn is held until the handoff binds, ends or expires.
use super::*;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Clock slack between the reservation stamp and the record Codex writes for the prompt.
const EVIDENCE_SKEW: chrono::TimeDelta = chrono::TimeDelta::seconds(1);
static HANDOFF_GENERATION: AtomicU64 = AtomicU64::new(1);
/// Serializes durable writes, so the file always mirrors the latest reservation state.
static DURABLE_WRITE: Mutex<()> = Mutex::new(());

/// The native turn and creation time a rollout user record names itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecordEvidence {
    pub turn_id: Option<String>,
    pub create_time: Option<DateTime<Utc>>,
}

impl RecordEvidence {
    pub fn from_record(json: &Value) -> Self {
        let meta = json.pointer("/payload/internal_chat_message_metadata_passthrough");
        let turn_id = meta
            .and_then(|meta| meta.get("turn_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|turn| !turn.is_empty())
            .map(str::to_string);
        let create_time = meta
            .and_then(|meta| meta.get("create_time"))
            .and_then(Value::as_f64)
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
            .and_then(|seconds| DateTime::from_timestamp_millis((seconds * 1000.0) as i64));
        Self {
            turn_id,
            create_time,
        }
    }

    /// Both the turn and the creation time; a partial record proves nothing.
    fn complete(&self) -> Option<(&str, DateTime<Utc>)> {
        Some((self.turn_id.as_deref()?, self.create_time?))
    }

    fn created_since(&self, since: DateTime<Utc>) -> bool {
        self.create_time
            .is_some_and(|created| created >= since - EVIDENCE_SKEW)
    }
}

/// How an observation saw its input.
#[derive(Clone, Copy, Debug)]
pub(super) enum Evidence<'a> {
    /// Non-Codex and legacy callers: never binds a handoff.
    Unattributed,
    /// A live Codex hook, naming its turn as it fired.
    Live,
    /// A rollout user record read in file order.
    Record(&'a RecordEvidence),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ParkedObservation {
    pub(super) prompt: String,
    pub(super) native_turn: Option<String>,
    pub(super) entry_ids: Vec<String>,
    pub(super) observed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct HandoffReservation {
    generation: u64,
    origin: String,
    reserved_wall: DateTime<Utc>,
    bound: Option<String>,
    parked: Vec<ParkedObservation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BindOutcome {
    NoReservation,
    NotOrigin,
    /// No turn and creation time: the reservation ends and behaves as before reservations.
    NoEvidence,
    /// A record older than the reservation: an earlier submission of the same text.
    Stale,
    Bound,
    /// Already bound to another turn.
    Conflict,
}

enum Proof<'a> {
    Live {
        turn: Option<&'a str>,
        at: DateTime<Utc>,
    },
    Record(&'a RecordEvidence),
}

/// Held from before a Discord prompt is submitted until its handoff tail returns.
#[must_use]
pub(crate) struct HandoffGuard {
    provider: String,
    tmux: String,
    generation: u64,
}

impl Drop for HandoffGuard {
    fn drop(&mut self) {
        if self.generation != 0 {
            release(&self.provider, &self.tmux, Some(self.generation));
        }
    }
}

/// Reserves `tmux`'s handoff for `origin`; call before the prompt can reach the pane.
pub(crate) fn reserve_discord_handoff(provider: &str, tmux: &str, origin: &str) -> HandoffGuard {
    let provider = normalize_provider(provider);
    let tmux = tmux.trim().to_string();
    if provider.is_empty() || tmux.is_empty() || origin.trim().is_empty() {
        return HandoffGuard {
            provider,
            tmux,
            generation: 0,
        };
    }
    let generation = HANDOFF_GENERATION.fetch_add(1, Ordering::Relaxed);
    let reservation = HandoffReservation {
        generation,
        origin: origin.to_string(),
        reserved_wall: Utc::now(),
        bound: None,
        parked: Vec::new(),
    };
    let replaced = {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state.purge_expired();
        let value = TimedValue {
            value: reservation,
            recorded_at: Instant::now(),
        };
        let previous = state
            .handoff_by_tmux
            .insert(PromptKey::new(&provider, &tmux), value);
        previous
            .map(|previous| previous.value.parked)
            .unwrap_or_default()
    };
    drain_parked(&provider, &tmux, replaced);
    sync_durable(&provider, &tmux);
    HandoffGuard {
        provider,
        tmux,
        generation,
    }
}

/// The handoff tail read its own origin record: binds the reservation on the record's evidence.
pub(crate) fn bind_discord_handoff_record(
    tmux: &str,
    prompt: &str,
    record: &RecordEvidence,
) -> BindOutcome {
    bind_origin("codex", tmux, &[prompt.to_string()], Proof::Record(record))
        .unwrap_or(BindOutcome::NotOrigin)
}

/// When one of `prompts` is the reserved origin, binds on the observation's evidence.
pub(super) fn bind_matching_origin(
    provider: &str,
    tmux: &str,
    prompts: &[String],
    native_turn: Option<&str>,
    evidence: Evidence<'_>,
    observed_at: DateTime<Utc>,
) -> Option<BindOutcome> {
    let proof = match evidence {
        Evidence::Unattributed => return None,
        Evidence::Live => Proof::Live {
            turn: native_turn,
            at: observed_at,
        },
        Evidence::Record(record) => Proof::Record(record),
    };
    bind_origin(provider, tmux, prompts, proof)
        .filter(|outcome| !matches!(outcome, BindOutcome::NotOrigin))
}

fn bind_origin(
    provider: &str,
    tmux: &str,
    prompts: &[String],
    proof: Proof<'_>,
) -> Option<BindOutcome> {
    let provider = normalize_provider(provider);
    let tmux = tmux.trim();
    let (outcome, drained) = {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state.purge_expired();
        let key = PromptKey::new(&provider, tmux);
        let origin = state.handoff_by_tmux.get(&key)?.value.origin.clone();
        if !prompts.iter().any(|prompt| prompts_match(&origin, prompt)) {
            return Some(BindOutcome::NotOrigin);
        }
        state.bind_handoff(&provider, tmux, proof)
    };
    if !matches!(outcome, BindOutcome::Stale | BindOutcome::Conflict) {
        tracing::info!(tmux, outcome = ?outcome, held = drained.len(), "discord handoff settled");
    }
    drain_parked(&provider, tmux, drained);
    sync_durable(&provider, tmux);
    Some(outcome)
}

/// Ends the reservation (`generation` None: whichever is current) and relays what it held.
fn release(provider: &str, tmux: &str, generation: Option<u64>) {
    let drained = {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state.purge_expired();
        let key = PromptKey::new(provider, tmux);
        let current = state
            .handoff_by_tmux
            .get(&key)
            .map(|entry| entry.value.generation);
        if current.is_none() || generation.is_some_and(|generation| current != Some(generation)) {
            return;
        }
        state
            .handoff_by_tmux
            .remove(&key)
            .map(|entry| entry.value.parked)
            .unwrap_or_default()
    };
    drain_parked(provider, tmux, drained);
    sync_durable(provider, tmux);
}

/// Relays the held observations of reservations that outlived their TTL.
pub(crate) fn drain_expired_handoffs() {
    let expired = {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state.purge_expired();
        std::mem::take(&mut state.handoff_expired)
    };
    for (key, parked) in expired {
        drain_parked(&key.provider, &key.key, parked);
        sync_durable(&key.provider, &key.key);
    }
}

/// Observes each held input exactly once, outside the STATE lock; a missing entry id stays None.
pub(super) fn drain_parked(provider: &str, tmux: &str, parked: Vec<ParkedObservation>) {
    for held in parked {
        let entry_id = held.entry_ids.first().map(String::as_str);
        let observation = observation::observe_held_prompt(
            provider,
            tmux,
            &held.prompt,
            entry_id,
            held.native_turn.as_deref(),
            held.observed_at,
        );
        for extra in held.entry_ids.iter().skip(1) {
            record_relayed_entry_id(provider, tmux, extra);
        }
        tracing::info!(tmux, observation = ?observation, "held Codex input observed after its handoff settled");
    }
}

impl TuiPromptDedupeState {
    /// An unbound reservation holds an observation that is not in file order.
    pub(super) fn park_handoff_observation(
        &mut self,
        key: &PromptKey,
        prompt: &str,
        entry_id: Option<&str>,
        native_turn: Option<&str>,
        observed_at: DateTime<Utc>,
    ) -> bool {
        let Some(entry) = self.handoff_by_tmux.get_mut(key) else {
            return false;
        };
        // The Discord prompt itself, seen without a turn, stays a Discord duplicate.
        if entry.value.bound.is_some() || prompts_match(&entry.value.origin, prompt) {
            return false;
        }
        let parked = &mut entry.value.parked;
        let same = parked.iter_mut().find(|held| {
            held.native_turn.as_deref() == native_turn && prompts_match(&held.prompt, prompt)
        });
        match same {
            Some(held) => {
                if let Some(entry_id) = entry_id
                    && !held.entry_ids.iter().any(|seen| seen == entry_id)
                {
                    held.entry_ids.push(entry_id.to_string());
                }
            }
            None => parked.push(ParkedObservation {
                prompt: prompt.to_string(),
                native_turn: native_turn.map(str::to_string),
                entry_ids: entry_id.map(str::to_string).into_iter().collect(),
                observed_at,
            }),
        }
        true
    }

    /// A record read in file order decides its own held copy, and a fresh record ahead of the
    /// origin ends the reservation: its turn cannot be the origin's. Returns what to relay.
    pub(super) fn settle_handoff_for_record(
        &mut self,
        key: &PromptKey,
        prompt: &str,
        record: &RecordEvidence,
    ) -> Vec<ParkedObservation> {
        let Some(entry) = self.handoff_by_tmux.get_mut(key) else {
            return Vec::new();
        };
        entry
            .value
            .parked
            .retain(|held| !prompts_match(&held.prompt, prompt));
        if entry.value.bound.is_some() || !record.created_since(entry.value.reserved_wall) {
            return Vec::new();
        }
        self.handoff_by_tmux
            .remove(key)
            .map(|entry| entry.value.parked)
            .unwrap_or_default()
    }

    fn bind_handoff(
        &mut self,
        provider: &str,
        tmux: &str,
        proof: Proof<'_>,
    ) -> (BindOutcome, Vec<ParkedObservation>) {
        let key = PromptKey::new(provider, tmux);
        let Some((bound, reserved_wall)) = self
            .handoff_by_tmux
            .get(&key)
            .map(|entry| (entry.value.bound.clone(), entry.value.reserved_wall))
        else {
            return (BindOutcome::NoReservation, Vec::new());
        };
        let (turn, at) = match proof {
            Proof::Live {
                turn: Some(turn),
                at,
            } => (turn.to_string(), at),
            Proof::Record(record) => match record.complete() {
                Some((turn, at)) => (turn.to_string(), at),
                None => return self.end_handoff(&key, BindOutcome::NoEvidence),
            },
            Proof::Live { turn: None, .. } => {
                return self.end_handoff(&key, BindOutcome::NoEvidence);
            }
        };
        if let Some(bound) = bound {
            let outcome = if bound == turn {
                BindOutcome::Bound
            } else {
                BindOutcome::Conflict
            };
            return (outcome, Vec::new());
        }
        if at < reserved_wall - EVIDENCE_SKEW {
            return (BindOutcome::Stale, Vec::new());
        }
        let relay_owned = self.native_turn_by_tmux.get(&key).is_some_and(|slot| {
            slot.value.turn == turn && slot.value.owner != NativeTurnOwner::Answer
        });
        if relay_owned {
            tracing::warn!(
                tmux,
                turn,
                "discord handoff joined a turn a direct input owns; its slot is kept"
            );
        } else {
            let answer = (
                SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
                NativeTurnOwner::Answer,
            );
            self.open_native_turn_slot(provider, tmux, &turn, answer);
        }
        let Some(entry) = self.handoff_by_tmux.get_mut(&key) else {
            return (BindOutcome::Bound, Vec::new());
        };
        entry.value.bound = Some(turn);
        // A held input naming no turn waits for its record to name one.
        let (named, unnamed) = std::mem::take(&mut entry.value.parked)
            .into_iter()
            .partition(|held| held.native_turn.is_some());
        entry.value.parked = unnamed;
        (BindOutcome::Bound, named)
    }

    fn end_handoff(
        &mut self,
        key: &PromptKey,
        outcome: BindOutcome,
    ) -> (BindOutcome, Vec<ParkedObservation>) {
        let parked = self
            .handoff_by_tmux
            .remove(key)
            .map(|entry| entry.value.parked)
            .unwrap_or_default();
        (outcome, parked)
    }

    /// Moves expired reservations' held inputs to the queue drained outside the lock.
    pub(super) fn expire_handoffs(&mut self, now: Instant) {
        let expired = self
            .handoff_by_tmux
            .iter()
            .filter(|(_, entry)| now.duration_since(entry.recorded_at) > PROMPT_ANCHOR_SUBMIT_TTL)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in expired {
            if let Some(entry) = self.handoff_by_tmux.remove(&key)
                && !entry.value.parked.is_empty()
            {
                self.handoff_expired.push((key, entry.value.parked));
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct DurableHandoff {
    provider: String,
    tmux: String,
    reservation: HandoffReservation,
}

fn durable_path(provider: &str, tmux: &str) -> Option<std::path::PathBuf> {
    let stem: String = format!("{provider}-{tmux}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    durable_root().map(|root| {
        root.join("runtime")
            .join("codex_handoff_held")
            .join(format!("{stem}.json"))
    })
}

#[cfg(not(test))]
fn durable_root() -> Option<std::path::PathBuf> {
    crate::config::runtime_root()
}

/// Tests mirror only under an explicit root that is not the live release runtime.
#[cfg(test)]
fn durable_root() -> Option<std::path::PathBuf> {
    let root = std::path::PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR")?);
    let live = dirs::home_dir().map(|home| home.join(".adk").join("release"));
    (!root.as_os_str().is_empty() && live.as_ref() != Some(&root)).then_some(root)
}

/// Mirrors an unbound reservation that holds inputs on disk, so a restart can restore it.
pub(super) fn sync_durable(provider: &str, tmux: &str) {
    let Some(path) = durable_path(provider, tmux) else {
        return;
    };
    let _write = DURABLE_WRITE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let record = {
        let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state
            .handoff_by_tmux
            .get(&PromptKey::new(provider, tmux))
            .filter(|entry| entry.value.bound.is_none() && !entry.value.parked.is_empty())
            .map(|entry| DurableHandoff {
                provider: provider.to_string(),
                tmux: tmux.to_string(),
                reservation: entry.value.clone(),
            })
    };
    let result = match record {
        Some(record) => serde_json::to_string(&record)
            .map_err(|error| error.to_string())
            .and_then(|data| crate::services::discord::runtime_store::atomic_write(&path, &data)),
        None => match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.to_string()),
            _ => Ok(()),
        },
    };
    if let Err(error) = result {
        tracing::warn!(tmux, path = %path.display(), error, "failed to mirror held Codex inputs");
    }
}

/// A restored Codex binding's scan start: the rollout's end, or the first user record created
/// since a reservation that held inputs across the restart, which is restored too.
pub(crate) fn codex_rehydrated_scan_offset(tmux: &str, rollout_path: &Path) -> u64 {
    let eof = std::fs::metadata(rollout_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let provider = normalize_provider("codex");
    let tmux = tmux.trim();
    let key = PromptKey::new(&provider, tmux);
    // Every restore path asks again; a reservation already held answers with the same floor.
    let holding = STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .handoff_by_tmux
        .get(&key)
        .map(|entry| {
            let holds = entry.value.bound.is_none() && !entry.value.parked.is_empty();
            holds.then_some(entry.value.reserved_wall)
        });
    let reserved_wall = match holding {
        Some(Some(reserved_wall)) => reserved_wall,
        Some(None) => return eof,
        None => {
            let Some(reservation) = durable_path(&provider, tmux)
                .and_then(|path| std::fs::read_to_string(path).ok())
                .and_then(|text| serde_json::from_str::<DurableHandoff>(&text).ok())
                .map(|record| record.reservation)
            else {
                return eof;
            };
            let reserved_wall = reservation.reserved_wall;
            let held = reservation.parked.len();
            let value = HandoffReservation {
                generation: HANDOFF_GENERATION.fetch_add(1, Ordering::Relaxed),
                ..reservation
            };
            let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
            state.handoff_by_tmux.entry(key).or_insert(TimedValue {
                value,
                recorded_at: Instant::now(),
            });
            tracing::info!(tmux, held, "restored a Codex handoff holding inputs");
            reserved_wall
        }
    };
    first_user_record_created_since(rollout_path, reserved_wall).map_or(eof, |floor| floor.min(eof))
}

fn first_user_record_created_since(rollout_path: &Path, since: DateTime<Utc>) -> Option<u64> {
    use std::io::BufRead;
    let file = std::fs::File::open(rollout_path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let (mut offset, mut line) = (0_u64, String::new());
    loop {
        line.clear();
        let read = reader.read_line(&mut line).ok()?;
        if read == 0 || !line.ends_with('\n') {
            return None;
        }
        let start = offset;
        offset += read as u64;
        let Ok(json) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if extract_codex_rollout_user_prompt_with_entry_id(&json).is_some()
            && RecordEvidence::from_record(&json).created_since(since)
        {
            return Some(start);
        }
    }
}

#[cfg(test)]
pub(crate) fn held_handoff_inputs_for_tests(provider: &str, tmux: &str) -> Option<usize> {
    let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state
        .handoff_by_tmux
        .get(&PromptKey::new(provider, tmux))
        .map(|entry| entry.value.parked.len())
}

#[cfg(test)]
pub(crate) fn durable_handoff_present_for_tests(provider: &str, tmux: &str) -> bool {
    durable_path(&normalize_provider(provider), tmux.trim()).is_some_and(|path| path.exists())
}

#[cfg(test)]
#[path = "handoff_tests.rs"]
pub(super) mod tests;
