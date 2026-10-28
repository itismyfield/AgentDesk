//! A prior process's path-less Herdr row ends once its provider is seen idle or its execution gone,
//! and a live turn's once its execution is gone. Every unclear read leaves the row as it was, and
//! says which gate left it.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::sync::LazyLock;
use std::sync::atomic::Ordering;

use super::herdr_before_start_restart::herdr_turn_held;
use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    self as hosted, HostedExecution, HostedLookup, HostedLookupKey, HostedObservation,
    HostedRecord, HostedState,
};
use crate::services::agent_protocol::NativeTerminalKind;
use crate::services::claude::herdr_turn::{HoldRelease, Terminal, TurnOutput, release_hold};
use crate::services::claude_tui::host_input::{HostCapture, HostInputOutcome, InputTarget};
use crate::services::discord::herdr_terminate::{ExecutionEnd, execution_end, retire_confirmed};
use crate::services::provider::session_probe::SessionLiveness;
use crate::services::session_host::{
    HerdrPaneView, HerdrTarget, PaneProvider, PaneReading, herdr_endpoints,
};
use crate::services::tui_prompt_dedupe::binding_events::{
    BindingTarget, SourceId, binding_events_since,
};
use outcome::{Outcome, Settle};

/// Boot restore keeps a held turn, and a row this leaf ends later, off the wrapper path.
pub(super) fn held_at_restore(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
) -> bool {
    if herdr_turn_held(provider, state) {
        return true;
    }
    let current = super::runtime_store::process_generation();
    let restarted = !mutant("restore") && target(provider, state, current);
    if restarted {
        tracing::info!(
            channel_id = state.channel_id,
            "recovery left a restarted Herdr row to its end pass"
        );
    }
    restarted
}

/// A prior process's Herdr row with no path, kind, receipt, planned hand-off or other owner.
fn target(provider: &ProviderKind, state: &inflight::InflightTurnState, current: u64) -> bool {
    hosted_provider(provider)
        && state.channel_id != 0
        && !inflight::row_is_current_generation(state, current)
        && state
            .turn_nonce
            .as_deref()
            .is_some_and(|nonce| !nonce.is_empty())
        && state.tui_terminal_kind.is_none()
        && state.herdr_before_start_stop.is_none()
        && state.output_path.as_deref().is_none_or(str::is_empty)
        && state.restart_mode != Some(InflightRestartMode::HotSwapHandoff)
        && !state.rebind_origin
        && !state.replay_rerun_blocked()
        && !state.runtime_kind_unknown_on_disk
        && !(state.runtime_kind.is_none() && state.version > inflight::inflight_state_version())
        && (mutant("herdr-only")
            || recovery_tmux_session_name(provider, state)
                .is_some_and(|name| super::turn_bridge::herdr_marked(&name)))
        && !inflight::before_start_receipt_present(provider, state.channel_id)
}

/// Claude, and Codex while its Herdr switch is on.
fn hosted_provider(provider: &ProviderKind) -> bool {
    match provider {
        ProviderKind::Claude => true,
        ProviderKind::Codex => crate::services::turn_host::herdr_turn_switched_on_for(provider),
        _ => false,
    }
}

/// Channels whose row an end task is reading now, with the phase it is in.
static ENDING: LazyLock<std::sync::Mutex<HashMap<(String, u64), diagnostics::Phase>>> =
    LazyLock::new(Default::default);

struct Ending((String, u64));

impl Ending {
    /// Claims the channel for one end task; `None` while another task reads it.
    fn claim(provider: &ProviderKind, channel: u64) -> Option<Self> {
        let key = (provider.as_str().to_owned(), channel);
        let mut ending = ENDING.lock().unwrap_or_else(|e| e.into_inner());
        if ending.contains_key(&key) {
            return None;
        }
        ending.insert(key.clone(), diagnostics::Phase::start());
        Some(Self(key))
    }
}

impl Drop for Ending {
    fn drop(&mut self) {
        ENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// Starts one end task per target row no task is reading; the pass itself never waits on one.
pub(in crate::services::discord) async fn row_end_pass(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    if mutant("tick") || !shared.restart.reconcile_done.load(Ordering::Acquire) {
        return;
    }
    let current = shared.restart.current_generation;
    for row in inflight::load_inflight_states(provider) {
        let live = live::live_target(provider, &row, current).is_some();
        if !(live || target(provider, &row, current)) {
            continue;
        }
        let Some(ending) = Ending::claim(provider, row.channel_id) else {
            if !live {
                diagnostics::ending_busy(provider, &row);
            }
            continue;
        };
        let (shared, provider) = (shared.clone(), provider.clone());
        if mutant("inline") {
            end_row(&shared, &provider, row).await;
            continue;
        }
        super::task_supervisor::spawn_observed("herdr_row_end", async move {
            let _ending = ending;
            match live {
                true => _ = live::end_live(&shared, &provider, row).await,
                false => end_row(&shared, &provider, row).await,
            }
        });
    }
}

/// Asks for the channel's end task at once, as a hold on its queue found the row: the task waits
/// for the asker's transition to drop, then reads the row as a pass would.
pub(in crate::services::discord) fn request_row_end(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) {
    let current = shared.restart.current_generation;
    let picked = inflight::load_inflight_state_read_only(provider, channel.get());
    let Some(picked) = picked.filter(|row| target(provider, row, current)) else {
        return;
    };
    let Some(ending) = Ending::claim(provider, channel.get()) else {
        return;
    };
    let (shared, provider) = (shared.clone(), provider.clone());
    super::task_supervisor::spawn_observed("herdr_row_end_request", async move {
        let _ending = ending;
        let transition = shared.session_transition_lock(channel).lock_owned().await;
        end_locked(&shared, &provider, picked, transition).await;
    });
}

/// Why an end left the row this time, as its diagnostics name the gate.
type Held = &'static str;

/// What the end found: I's idle provider or E's ended execution, and the input's own turn.
struct Seen {
    idle: bool,
    outcome: Outcome,
    nonce: String,
    /// The live execution an E retires.
    retire: Option<(HostedObservation, HostedExecution)>,
}

async fn end_row(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    picked: inflight::InflightTurnState,
) {
    let channel = ChannelId::new(picked.channel_id);
    // A held transition is another owner's turn change; the next pass reads the row again.
    match shared.session_transition_lock(channel).try_lock_owned() {
        Ok(transition) => end_locked(shared, provider, picked, transition).await,
        Err(_) => diagnostics::held(provider, &picked, "transition_busy"),
    }
}

async fn end_locked(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    picked: inflight::InflightTurnState,
    transition: tokio::sync::OwnedMutexGuard<()>,
) {
    if let Err(reason) = settle_row(shared, provider, &picked, transition).await {
        diagnostics::held(provider, &picked, reason);
    }
}

async fn settle_row(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    picked: &inflight::InflightTurnState,
    transition: tokio::sync::OwnedMutexGuard<()>,
) -> Result<(), Held> {
    let channel = ChannelId::new(picked.channel_id);
    let current = shared.restart.current_generation;
    let row = match inflight::load_inflight_state_read_only_result(provider, channel.get()) {
        Ok(Some(row)) => row,
        Ok(None) => return Err("row_missing"),
        Err(_) => return Err("row_read_error"),
    };
    if row.turn_nonce != picked.turn_nonce || row.save_generation != picked.save_generation {
        return Err("fresh_mismatch");
    }
    if !target(provider, &row, current) {
        return Err("cleanup_owner_changed");
    }
    // A prior process's turn has no actor here; one present now is another turn's.
    if !mutant("actor") {
        actor_absent(shared, provider, channel, &row).await?;
    }
    let pool = shared.pg_pool.as_ref().ok_or("pool_unavailable")?;
    diagnostics::phase(provider, channel.get(), "observe");
    let seen = observe(pool, &shared.token_hash, provider, &row).await?;
    let o_owned = o_owns_output(&row).ok_or("o_owner_unreadable")?;
    let settle = outcome::settle(provider, &row, &seen.outcome, seen.idle, o_owned);
    let silent = settle == Settle::Silent;
    if silent {
        diagnostics::phase(provider, channel.get(), "commit");
        commit(pool, &seen).await?;
    } else {
        let http = shared
            .serenity_http_or_token_fallback()
            .ok_or("http_unavailable")?;
        // Routing gates every write, as it gates boot restore's notice.
        diagnostics::phase(provider, channel.get(), "routing");
        if !routed(&http, shared, provider, channel, &row).await {
            return Err("routed_false");
        }
        diagnostics::phase(provider, channel.get(), "commit");
        commit(pool, &seen).await?;
        diagnostics::phase(provider, channel.get(), "notice");
        if notice(&http, shared, provider, &row, settle).await != RecoveryRelayOutcome::Delivered {
            kept(&row, "notice not delivered");
            return Err("notice_not_delivered");
        }
    }
    let report = super::restart_report::load_restart_report(provider, row.channel_id);
    #[cfg(test)]
    tests::hook("before_clear").await;
    if o_owned {
        super::super::outbound::completed_turn_ledger::o_pending::capture(provider, &row).await;
    }
    diagnostics::phase(provider, channel.get(), "clear");
    let (snapshot, owner) = (row.clone(), provider.clone());
    let clear = move || snapshot.clear_restarted_herdr_row(&owner, current, silent);
    let cleared = tokio::task::spawn_blocking(clear).await;
    if !matches!(cleared, Ok(inflight::GuardedClearOutcome::Cleared)) {
        kept(&row, "the fresh row is not this turn");
        return Err("clear_rejected");
    }
    super::restart_report::clear_loaded_restart_report(report.as_ref());
    diagnostics::settled(provider, &row);
    drop(transition);
    super::schedule_deferred_idle_queue_kickoff_immediate(
        shared.clone(),
        provider.clone(),
        channel,
        "herdr_row_end",
    );
    if silent {
        placeholder::spawn_cleanup(shared, provider, &row);
    }
    Ok(())
}

fn kept(row: &inflight::InflightTurnState, why: &str) {
    tracing::warn!(
        channel_id = row.channel_id,
        why,
        "herdr row end kept the row"
    );
}

/// Whether O owns the row channel's output, for the kind the row recorded; `None` when unread.
fn o_owns_output(row: &inflight::InflightTurnState) -> Option<bool> {
    let peek = crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel;
    peek(row.channel_id, row.runtime_kind).ok()
}

/// The recovery notice, or the neutral one through transport alone: on the turn's own card, else as
/// a new message once that card is gone.
async fn notice(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    settle: Settle,
) -> RecoveryRelayOutcome {
    let text = match settle {
        Settle::Notice(text) => {
            return relay_recovery_body_notice(http, shared, provider, row, &text).await;
        }
        Settle::Neutral(text) => text,
        Settle::Silent => return RecoveryRelayOutcome::Delivered,
    };
    let channel = ChannelId::new(row.channel_id);
    let card = inflight::opt_message_id(row.current_msg_id);
    match relay_recovered_terminal_text_to_placeholder(http, shared, channel, card, &text, None)
        .await
    {
        RecoveryRelayOutcome::PermanentFailure if card.is_some() => {
            relay_recovered_terminal_text_to_placeholder(http, shared, channel, None, &text, None)
                .await
        }
        outcome => outcome,
    }
}

/// Neither the channel's mailbox holds a turn nor a live Herdr turn holds this row's nonce.
async fn actor_absent(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    row: &inflight::InflightTurnState,
) -> Result<(), Held> {
    if let Some(mailbox) = shared.mailbox_peek(channel)
        && !matches!(mailbox.cancel_token().await, Ok(None))
    {
        return Err("actor_mailbox");
    }
    let nonce = row.turn_nonce.as_deref().unwrap_or_default();
    let logical = recovery_tmux_session_name(provider, row).ok_or("actor_unresolved")?;
    let indexed = crate::services::provider::cancel_token_claude_interrupt::herdr_turn_indexed;
    match indexed(&logical, nonce) {
        true => Err("actor_herdr_index"),
        false => Ok(()),
    }
}

/// Boot restore's routing check, on the same inputs.
async fn routed(
    http: &Arc<serenity::Http>,
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    row: &inflight::InflightTurnState,
) -> bool {
    let tmux = recovery_tmux_session_name(provider, row);
    let name = row.channel_name.clone().or_else(|| {
        let (_, channel) =
            crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux.as_deref()?)?;
        Some(channel)
    });
    let (allowed, provider_name) = match super::resolve_thread_parent(http, channel).await {
        Some((parent, parent_name)) => (parent, parent_name.or(name.clone())),
        None => (channel, name.clone()),
    };
    let is_dm = matches!(
        channel.to_channel(http).await,
        Ok(serenity::model::channel::Channel::Private(_))
    );
    let settings = shared.settings.read().await;
    validate_bot_channel_routing_with_provider_channel(
        &settings,
        provider,
        allowed,
        name.as_deref(),
        provider_name.as_deref(),
        is_dm,
    )
    .is_ok()
}

/// E first, then I, from reads alone; after E the input's turn is read from its source without the
/// composer.
async fn observe(
    pool: &sqlx::PgPool,
    token_hash: &str,
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
) -> Result<Seen, Held> {
    let (observation, record) = hosted_record(pool, token_hash, provider, row).await?;
    let nonce = record.execution_nonce.clone();
    let logical = record.owner.logical_key.clone();
    let retire = match record.state {
        HostedState::Retired => None,
        _ => {
            let target = herdr_endpoints()
                .target(&record)
                .ok_or("endpoint_unavailable")?;
            match execution_end(target.clone(), record.clone()).await {
                ExecutionEnd::ProviderAlive if !mutant("exited-only") => {
                    return Err("provider_alive_after_pane_exit");
                }
                ExecutionEnd::Ended | ExecutionEnd::ProviderAlive => Some((observation, record)),
                ExecutionEnd::Unconfirmed => {
                    let outcome = idle(provider, row, &record, target, &nonce).await?;
                    return Ok(Seen {
                        idle: true,
                        outcome,
                        nonce,
                        retire: None,
                    });
                }
            }
        }
    };
    let source = logged_source(row.channel_id, &logical, &nonce).ok();
    let source = source.filter(|source| same_session(row, source));
    let (kind, submitted) = (provider.clone(), submitted_at(row));
    let read = move || ended_read(&kind, source.as_ref(), submitted);
    let outcome = tokio::task::spawn_blocking(read)
        .await
        .unwrap_or(Outcome::Unknown);
    Ok(Seen {
        idle: false,
        outcome,
        nonce,
        retire,
    })
}

/// I on the execution `record` pins.
async fn idle(
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    record: &HostedExecution,
    target: HerdrTarget,
    nonce: &str,
) -> Result<Outcome, Held> {
    let view = herdr_endpoints()
        .view(record)
        .ok_or("endpoint_unavailable")?;
    let source = logged_source(row.channel_id, &record.owner.logical_key, nonce)?;
    if !same_session(row, &source) {
        return Err("session_mismatch");
    }
    let expected = record.expected.clone().ok_or("execution_unpinned")?;
    let (kind, submitted) = (provider.clone(), submitted_at(row));
    let read = move || idle_read(&kind, &view, &target, &expected, &source, submitted);
    tokio::task::spawn_blocking(read)
        .await
        .map_err(|_| "source_unreadable")?
}

fn same_session(row: &inflight::InflightTurnState, source: &SourceId) -> bool {
    (row.session_id.as_deref()).is_none_or(|id| id == source.session_id)
}

fn submitted_at(row: &inflight::InflightTurnState) -> Option<chrono::DateTime<chrono::Utc>> {
    inflight::parse_started_at_unix(&row.started_at)
        .and_then(|at| chrono::DateTime::from_timestamp(at, 0))
}

/// After E: the input's turn as the logged source shows it; only a Claude transcript with the
/// input's time can name it.
fn ended_read(
    provider: &ProviderKind,
    source: Option<&SourceId>,
    submitted: Option<chrono::DateTime<chrono::Utc>>,
) -> Outcome {
    use crate::services::claude::herdr_turn::ended_turn_read;
    let read = || {
        let (source, submitted) = (source?, submitted?);
        if *provider != ProviderKind::Claude {
            return None;
        }
        let mut file = std::fs::File::open(&source.path).ok()?;
        let meta = file.metadata().ok()?;
        let same = (meta.dev(), meta.ino()) == (source.dev, source.ino);
        let read = ended_turn_read(&mut file, submitted, meta.len());
        same.then_some(read.ok()?.into())
    };
    read().unwrap_or(Outcome::Unknown)
}

/// Retires an E's execution by CAS, then releases the execution's input hold.
async fn commit(pool: &sqlx::PgPool, seen: &Seen) -> Result<(), Held> {
    if let Some((observation, record)) = &seen.retire {
        #[cfg(test)]
        tests::hook("before_retire").await;
        let retired = retire_confirmed(pool, observation, record).await;
        retired.map_err(|_| "retire_cas_failed")?;
    }
    if seen.idle && mutant("hold") {
        return Ok(());
    }
    match release_hold(&seen.nonce) {
        HoldRelease::Released => Ok(()),
        HoldRelease::NotDurable(_) => Err("hold_not_durable"),
        HoldRelease::Kept(_) => Err("hold_kept"),
    }
}

/// The channel's hosted record, pinned to the row's logical session and recorded execution.
async fn hosted_record(
    pool: &sqlx::PgPool,
    token_hash: &str,
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
) -> Result<(HostedObservation, HostedExecution), Held> {
    let channel = row.channel_id.to_string();
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: token_hash,
        channel_id: &channel,
    };
    let key = HostedLookupKey::Canonical {
        provider: provider.as_str(),
        identity,
    };
    let HostedLookup::Found(found) = hosted::load_hosted_execution_pg(pool, key).await else {
        return Err("hosted_unavailable");
    };
    let HostedRecord::Known(record) = found.record.clone() else {
        return Err("hosted_unavailable");
    };
    let pinned = recovery_tmux_session_name(provider, row).as_deref()
        == Some(record.owner.logical_key.as_str())
        && (row.hosted_execution_nonce.as_ref())
            .is_none_or(|nonce| *nonce == record.execution_nonce)
        && row
            .hosted_record_id
            .is_none_or(|id| id == found.session_id());
    pinned
        .then_some((found, record))
        .ok_or("execution_unpinned")
}

/// The pane's latest logged source when execution `nonce` logged it, as source reconnect reads it.
fn logged_source(channel: u64, logical: &str, nonce: &str) -> Result<SourceId, Held> {
    let events = binding_events_since(channel, 0).map_err(|_| "source_unreadable")?;
    let last = events
        .into_iter()
        .filter(|event| event.tmux_session == logical)
        .filter(|event| !matches!(event.new, BindingTarget::Rejected { .. }))
        .last()
        .ok_or("source_none")?;
    if last.execution_nonce.as_deref() != Some(nonce) {
        return Err("source_nonce_mismatch");
    }
    match last.new {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => Ok(source),
        _ => Err("source_pending"),
    }
}

/// I on one execution read before and after: its own provider runs, the composer is ready with no
/// draft or modal, and the source's last turn closed; then the input's own turn as it reads.
fn idle_read(
    provider: &ProviderKind,
    view: &HerdrPaneView,
    target: &HerdrTarget,
    expected: &hosted::ExpectedExecution,
    source: &SourceId,
    submitted: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Outcome, Held> {
    let before = view.read_execution();
    let PaneReading::Present {
        root,
        provider: PaneProvider::Execution(running),
    } = &before
    else {
        return Err("exec_changed");
    };
    if *root != expected.root || *running != expected.provider_process {
        return Err("exec_changed");
    }
    let screen = target.capture(-80).ok_or("composer_unreadable")?;
    match provider {
        ProviderKind::Claude => {
            let input = InputTarget::Herdr(target.clone());
            let capture = HostCapture::Complete(screen);
            let classify = crate::services::claude_tui::host_input::classify;
            match classify(&input, SessionLiveness::Alive, &capture) {
                HostInputOutcome::ReadySameExecution => {}
                HostInputOutcome::Busy => return Err("composer_busy"),
                HostInputOutcome::PersistentDraft => return Err("composer_draft"),
                _ => return Err("composer_unreadable"),
            }
        }
        ProviderKind::Codex => {
            use crate::services::codex_tui::input::{self as codex, ComposerContent};
            if !codex::pane_looks_ready_for_codex_prompt(&screen) {
                return Err("composer_busy");
            }
            if !mutant("no-empty-composer")
                && codex::active_composer_content_in_pane(&screen) != ComposerContent::Empty
            {
                return Err("composer_draft");
            }
        }
        _ => return Err("composer_unreadable"),
    }
    let mut file = std::fs::File::open(&source.path).map_err(|_| "source_unreadable")?;
    let meta = file.metadata().map_err(|_| "source_unreadable")?;
    if (meta.dev(), meta.ino()) != (source.dev, source.ino) {
        return Err("file_identity");
    }
    let len = meta.len();
    let outcome = match provider {
        ProviderKind::Claude => {
            let read = crate::services::claude::herdr_turn::closed_turn_read;
            read(&mut file, submitted, len)
                .ok_or("transcript_open")?
                .map_or(Outcome::Unknown, Outcome::from)
        }
        // A rollout shows its latest turn closed, never where the input's own turn began.
        _ => match codex_last_turn_closed(&mut file, len) {
            Some(true) => Outcome::Unknown,
            Some(false) => return Err("transcript_open"),
            None => return Err("source_unreadable"),
        },
    };
    match view.read_execution() == before {
        true => Ok(outcome),
        false => Err("exec_changed"),
    }
}

/// A completed turn's reply: its explicit result, else its streamed text composed as the live bridge
/// composes it, with one paragraph break at each tool use.
fn turn_body(turn: Option<Terminal>) -> Option<String> {
    use crate::services::discord::semantic_boundaries::{
        semantic_chunk_separator_needed, text_ends_inside_open_code_fence,
    };
    let turn = turn.filter(|turn| turn.kind == NativeTerminalKind::Completed)?;
    if !turn.result.trim().is_empty() || mutant("done-overwrites") {
        return Some(turn.result).filter(|result| !result.trim().is_empty());
    }
    let mut body = String::new();
    for part in turn.output.iter().filter(|_| !mutant("no-body-collect")) {
        match part {
            TurnOutput::Tool if !body.is_empty() => {
                body.truncate(body.trim_end().len());
                body.push_str("\n\n");
            }
            TurnOutput::Tool => {}
            TurnOutput::Text(text)
                if body.ends_with("\n\n") && !text_ends_inside_open_code_fence(&body) =>
            {
                body.push_str(text.trim_start_matches('\n'));
            }
            TurnOutput::Text(text) => {
                if semantic_chunk_separator_needed(&body, text) {
                    body.push_str("\n\n");
                }
                body.push_str(text);
            }
        }
    }
    (!body.trim().is_empty()).then_some(body)
}

/// The rollout's latest started turn has its own completion or abort and no input after it; a torn
/// tail is a record still being written.
fn codex_last_turn_closed(file: &mut std::fs::File, len: u64) -> Option<bool> {
    use crate::services::agent_protocol::{codex_payload_turn_id, same_codex_turn};
    let mut bytes = Vec::new();
    file.take(len).read_to_end(&mut bytes).ok()?;
    let tail = bytes
        .rsplit(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    if !tail.trim_ascii().is_empty() {
        return Some(false);
    }
    let (mut started, mut closed) = (None::<String>, false);
    for line in bytes.split(|byte| *byte == b'\n') {
        let Ok(json) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if json["type"] != "event_msg" {
            continue;
        }
        let turn = codex_payload_turn_id(&json["payload"]).map(str::to_owned);
        match json["payload"]["type"].as_str() {
            Some("task_started") => (started, closed) = (turn, false),
            Some("task_complete" | "turn_aborted") => {
                closed |= same_codex_turn(started.as_deref(), turn.as_deref());
            }
            Some("user_message") => closed = false,
            _ => {}
        }
    }
    Some(closed)
}

/// A test build's mutant switch; always off otherwise.
fn mutant(name: &str) -> bool {
    cfg!(test) && std::env::var("ADK_HERDR_ROW_END_MUTANT").as_deref() == Ok(name)
}

#[path = "herdr_row_end/diagnostics.rs"]
mod diagnostics;
#[path = "herdr_row_end_live.rs"]
mod live;
#[path = "herdr_row_end/outcome.rs"]
mod outcome;
#[path = "herdr_row_end/owner.rs"]
pub(in crate::services::discord) mod owner;
#[path = "herdr_row_end/placeholder.rs"]
mod placeholder;

#[cfg(test)]
#[path = "herdr_row_end_tests.rs"]
pub(in crate::services::discord) mod tests;
