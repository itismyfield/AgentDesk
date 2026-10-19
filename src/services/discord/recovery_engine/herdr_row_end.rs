//! A prior process's path-less Herdr row ends once its provider is seen idle or its execution gone,
//! and a live turn's once its execution is gone. Every unclear read leaves the row as it was.

use std::collections::HashSet;
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

/// Channels whose row an end task is reading now.
static ENDING: LazyLock<std::sync::Mutex<HashSet<(String, u64)>>> = LazyLock::new(Default::default);

struct Ending((String, u64));

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
        let key = (provider.as_str().to_owned(), row.channel_id);
        let live = live::live_target(provider, &row, current).is_some();
        if !(live || target(provider, &row, current))
            || !ENDING
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone())
        {
            continue;
        }
        let ending = Ending(key);
        let (shared, provider) = (shared.clone(), provider.clone());
        if mutant("inline") {
            match live {
                true => live::end_live(&shared, &provider, row).await,
                false => end_row(&shared, &provider, row).await,
            }
            continue;
        }
        super::task_supervisor::spawn_observed("herdr_row_end", async move {
            let _ending = ending;
            match live {
                true => live::end_live(&shared, &provider, row).await,
                false => end_row(&shared, &provider, row).await,
            }
        });
    }
}

/// How the row's execution was seen to end.
enum Ended {
    /// Retired here or before: the notice says the turn was cut off.
    Gone,
    /// The provider went idle; `body` is the turn's own completed reply when it read cleanly.
    Idle { body: Option<String> },
}

async fn end_row(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    picked: inflight::InflightTurnState,
) {
    let channel = ChannelId::new(picked.channel_id);
    // A held transition is another owner's turn change; the next pass reads the row again.
    let Ok(transition) = shared.session_transition_lock(channel).try_lock_owned() else {
        return;
    };
    let current = shared.restart.current_generation;
    let fresh = inflight::load_inflight_state_read_only(provider, channel.get());
    let Some(row) = fresh.filter(|row| {
        row.turn_nonce == picked.turn_nonce
            && row.save_generation == picked.save_generation
            && target(provider, row, current)
    }) else {
        return;
    };
    // A prior process's turn has no actor here; one present now is another turn's.
    if !mutant("actor") && actor_present(shared, provider, channel, &row).await {
        return;
    }
    let Some(pool) = shared.pg_pool.as_ref() else {
        return;
    };
    let Some(seen) = observe(pool, &shared.token_hash, provider, &row).await else {
        return;
    };
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return;
    };
    // Routing gates every write, as it gates boot restore's notice.
    if !routed(&http, shared, provider, channel, &row).await {
        return;
    }
    let Some(ended) = commit(pool, seen).await else {
        return;
    };
    let text = match ended {
        Ended::Idle { body: Some(body) } if !mutant("body") => {
            super::formatting::format_for_discord_with_provider(&body, provider)
        }
        _ => interrupted_recovery_message(&row, &row.full_response),
    };
    let outcome = relay_recovery_body_notice(&http, shared, provider, &row, &text).await;
    if outcome != RecoveryRelayOutcome::Delivered {
        return kept(&row, "notice not delivered");
    }
    let report = super::restart_report::load_restart_report(provider, row.channel_id);
    #[cfg(test)]
    tests::hook("before_clear").await;
    let (snapshot, owner) = (row.clone(), provider.clone());
    let cleared =
        tokio::task::spawn_blocking(move || snapshot.clear_restarted_herdr_row(&owner, current))
            .await;
    if !matches!(cleared, Ok(inflight::GuardedClearOutcome::Cleared)) {
        return kept(&row, "the fresh row is not this turn");
    }
    super::restart_report::clear_loaded_restart_report(report.as_ref());
    tracing::info!(
        channel_id = row.channel_id,
        "herdr row end settled a restarted turn"
    );
    drop(transition);
    super::schedule_deferred_idle_queue_kickoff(
        shared.clone(),
        provider.clone(),
        channel,
        "herdr_row_end",
    );
}

fn kept(row: &inflight::InflightTurnState, why: &str) {
    tracing::warn!(
        channel_id = row.channel_id,
        why,
        "herdr row end kept the row"
    );
}

/// The channel's mailbox holds a turn, or a live Herdr turn holds this row's nonce.
async fn actor_present(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    row: &inflight::InflightTurnState,
) -> bool {
    if let Some(mailbox) = shared.mailbox_peek(channel)
        && !matches!(mailbox.cancel_token().await, Ok(None))
    {
        return true;
    }
    let nonce = row.turn_nonce.as_deref().unwrap_or_default();
    recovery_tmux_session_name(provider, row).is_none_or(|logical| {
        crate::services::provider::cancel_token_claude_interrupt::herdr_turn_indexed(
            &logical, nonce,
        )
    })
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

/// What the reads showed; nothing is written before routing admits the row.
struct Seen {
    ended: Ended,
    nonce: String,
    /// The live execution an E retires.
    retire: Option<(HostedObservation, HostedExecution)>,
}

/// E first, then I, from reads alone.
async fn observe(
    pool: &sqlx::PgPool,
    token_hash: &str,
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
) -> Option<Seen> {
    let (observation, record) = hosted_record(pool, token_hash, provider, row).await?;
    let nonce = record.execution_nonce.clone();
    if record.state == HostedState::Retired {
        return Some(Seen {
            ended: Ended::Gone,
            nonce,
            retire: None,
        });
    }
    let target = herdr_endpoints().target(&record)?;
    match execution_end(target.clone(), record.clone()).await {
        ExecutionEnd::ProviderAlive if !mutant("exited-only") => return None,
        ExecutionEnd::Ended | ExecutionEnd::ProviderAlive => {
            let retire = Some((observation, record));
            return Some(Seen {
                ended: Ended::Gone,
                nonce,
                retire,
            });
        }
        ExecutionEnd::Unconfirmed => {}
    }
    let view = herdr_endpoints().view(&record)?;
    let source = logged_source(row.channel_id, &record.owner.logical_key, &nonce)?;
    if row
        .session_id
        .as_deref()
        .is_some_and(|id| id != source.session_id)
    {
        return None;
    }
    let submitted = inflight::parse_started_at_unix(&row.started_at)
        .and_then(|at| chrono::DateTime::from_timestamp(at, 0));
    let expected = record.expected.clone()?;
    let kind = provider.clone();
    let read = move || idle_read(&kind, &view, &target, &expected, &source, submitted);
    let body = tokio::task::spawn_blocking(read).await.ok()??;
    let ended = Ended::Idle { body };
    Some(Seen {
        ended,
        nonce,
        retire: None,
    })
}

/// Retires an E's execution by CAS, then releases the execution's input hold.
async fn commit(pool: &sqlx::PgPool, seen: Seen) -> Option<Ended> {
    if let Some((observation, record)) = &seen.retire {
        #[cfg(test)]
        tests::hook("before_retire").await;
        retire_confirmed(pool, observation, record).await.ok()?;
    }
    let keep_hold = matches!(seen.ended, Ended::Idle { .. }) && mutant("hold");
    (keep_hold || release_hold(&seen.nonce) == HoldRelease::Released).then_some(seen.ended)
}

/// The channel's hosted record, pinned to the row's logical session and recorded execution.
async fn hosted_record(
    pool: &sqlx::PgPool,
    token_hash: &str,
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
) -> Option<(HostedObservation, HostedExecution)> {
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
        return None;
    };
    let HostedRecord::Known(record) = found.record.clone() else {
        return None;
    };
    let pinned = recovery_tmux_session_name(provider, row).as_deref()
        == Some(record.owner.logical_key.as_str())
        && (row.hosted_execution_nonce.as_ref())
            .is_none_or(|nonce| *nonce == record.execution_nonce)
        && row
            .hosted_record_id
            .is_none_or(|id| id == found.session_id());
    pinned.then_some((found, record))
}

/// The pane's latest logged source when execution `nonce` logged it, as source reconnect reads it.
fn logged_source(channel: u64, logical: &str, nonce: &str) -> Option<SourceId> {
    let events = binding_events_since(channel, 0).ok()?;
    let last = events
        .into_iter()
        .filter(|event| event.tmux_session == logical)
        .filter(|event| !matches!(event.new, BindingTarget::Rejected { .. }))
        .last()?;
    if last.execution_nonce.as_deref() != Some(nonce) {
        return None;
    }
    match last.new {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => Some(source),
        _ => None,
    }
}

/// I on one execution read before and after: its own provider runs, the composer is ready with no
/// draft or modal, and the source's last turn closed. `Some(body)` names the turn's own reply.
fn idle_read(
    provider: &ProviderKind,
    view: &HerdrPaneView,
    target: &HerdrTarget,
    expected: &hosted::ExpectedExecution,
    source: &SourceId,
    submitted: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<Option<String>> {
    let before = view.read_execution();
    let PaneReading::Present {
        root,
        provider: PaneProvider::Execution(running),
    } = &before
    else {
        return None;
    };
    if *root != expected.root || *running != expected.provider_process {
        return None;
    }
    let screen = target.capture(-80)?;
    let ready = match provider {
        ProviderKind::Claude => {
            let input = InputTarget::Herdr(target.clone());
            let capture = HostCapture::Complete(screen);
            let outcome = crate::services::claude_tui::host_input::classify(
                &input,
                SessionLiveness::Alive,
                &capture,
            );
            outcome == HostInputOutcome::ReadySameExecution
        }
        ProviderKind::Codex => {
            use crate::services::codex_tui::input::{self as codex, ComposerContent};
            codex::pane_looks_ready_for_codex_prompt(&screen)
                && (mutant("no-empty-composer")
                    || codex::active_composer_content_in_pane(&screen) == ComposerContent::Empty)
        }
        _ => false,
    };
    let mut file = std::fs::File::open(&source.path).ok().filter(|_| ready)?;
    let meta = file.metadata().ok()?;
    if (meta.dev(), meta.ino()) != (source.dev, source.ino) {
        return None;
    }
    let len = meta.len();
    let body = match provider {
        ProviderKind::Claude => turn_body(crate::services::claude::herdr_turn::closed_turn(
            &mut file, submitted, len,
        )?),
        _ => codex_last_turn_closed(&mut file, len)?.then_some(None)?,
    };
    (view.read_execution() == before).then_some(body)
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

#[path = "herdr_row_end_live.rs"]
mod live;

#[cfg(test)]
#[path = "herdr_row_end_tests.rs"]
mod tests;
