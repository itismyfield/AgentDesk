//! The restart itself, under the channel's transition guard from capture to admission.
use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::{notice, witness};
use crate::services::discord::SharedData;
use crate::services::discord::queue_io::input_owner::ingress::Intake;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::PendingQueueItem;
use crate::services::turn_orchestrator::effort_intent::{Cut, EffortRestart, PROMPT, Request};

/// What `/effort` did to the channel's TUI after its level was saved.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum Restart {
    /// The TUI ended. `continues`: a turn was cut and its continuation is admitted, so the owner
    /// resumes the session now, else the next input does. `told`: that turn's notice is staged.
    Restarted { continues: bool, told: bool },
    /// The TUI ended and its turn was cut; the continuation's admission is not confirmed yet and
    /// is asked again, under the same ticket, on the owner's next wake.
    Admitting { told: bool },
    /// No TUI runs: the next launch takes the level.
    NotRunning,
    /// Nothing was ended, for this reason.
    Kept(String),
    /// The TUI ended and its turn was cut, but the continuation is not admitted.
    Unfinished(String),
}

/// Where a restart's intent stands after one pass of the steps left once its TUI ended.
pub(super) enum Step {
    Admitted {
        told: bool,
    },
    Admitting {
        told: bool,
    },
    /// The old process's end is not recorded yet: the intent stays prepared and is retried.
    Unproven(String),
    Cancelled,
    Refused(String),
}

type Intent = EffortRestart<PendingQueueItem>;

/// The channel's stored restart intent, if its owner holds one.
pub(super) async fn stored(shared: &Arc<SharedData>, channel: ChannelId) -> Option<Intent> {
    let generation = crate::services::discord::runtime_store::process_generation();
    let mailbox = shared.mailbox_peek(channel)?;
    let read = mailbox.v2_request(move |slot| match slot.as_mut() {
        Some(owner) => Ok(owner.prepare_effort(None, channel.get(), generation).ok()),
        None => Ok(None),
    });
    read.await.ok().flatten()
}

/// The channel's intent while it still owes a step its restart did not finish.
pub(super) async fn owed(shared: &Arc<SharedData>, channel: ChannelId) -> Option<Intent> {
    let mailbox = shared.mailbox_peek(channel)?;
    let read = mailbox.v2_request(|slot| Ok(slot.as_ref().and_then(|owner| owner.effort_owed())));
    read.await.ok().flatten()
}

/// Ends the channel's live TUI now and settles only the turn captured on it. A captured turn
/// gets one continuation on the same session; with none captured, nothing is owed.
pub(in crate::services::discord) async fn restart(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Restart {
    use super::super::tmux_recreate::{Relaunch, end_channel_tmux_keeping_session};
    use crate::services::discord::herdr_terminate::{capture_turn, settle_captured};
    // Declared before the guard: the owner is woken once the guard is released.
    let _completion = shared.session_transition_completion(channel);
    let Ok(_transition) = shared.acquire_session_transition(channel).await else {
        return Restart::Kept("다른 세션 전환이 진행 중입니다".into());
    };
    let named = {
        let core = shared.core.lock().await;
        let session = core.sessions.get(&channel);
        session.and_then(|session| {
            let name = session.channel_name.as_deref()?;
            let tmux = provider.build_tmux_session_name(name);
            Some((tmux, session.session_id.clone()))
        })
    };
    let Some((tmux, native_session)) = named else {
        return Restart::NotRunning;
    };
    if !crate::services::tmux_diagnostics::tmux_session_has_live_pane(&tmux) {
        return Restart::NotRunning;
    }
    let Some(native_session) = native_session else {
        return Restart::Kept("이 채널의 세션 ID를 확인하지 못했습니다".into());
    };
    let turn = match capture_turn(shared, provider, channel).await {
        Ok(turn) => turn,
        Err(reason) => {
            return Restart::Kept(format!("진행 중인 턴을 특정하지 못했습니다({reason})"));
        }
    };
    // A continuation not yet submitted keeps its own episode; a running turn would be another one.
    let waiting = stored(shared, channel)
        .await
        .filter(|intent| intent.pending().is_some());
    if let Some(intent) = &waiting
        && (turn.is_some() || intent.native_session != native_session)
    {
        return Restart::Kept("앞선 재기동의 자동 이어하기가 아직 시작되지 않았습니다".into());
    }
    if (turn.is_some() || waiting.is_some()) && !owner_takes(shared, provider, channel).await {
        return Restart::Kept(
            "이 채널의 입력 경로는 진행 중 턴의 즉시 재기동을 지원하지 않습니다".into(),
        );
    }
    // Before anything ends, the cut turn's continuation and the process ending are stored, so a
    // server stopping at any later step still owes that continuation, or cancels it.
    let operation = match (&turn, waiting) {
        (Some(turn), _) => {
            let Some(episode) = turn.nonce() else {
                return Restart::Kept("진행 중인 턴의 식별자를 확인하지 못했습니다".into());
            };
            let operation = uuid::Uuid::new_v4().simple().to_string();
            let request = prepared(
                provider,
                channel,
                &tmux,
                (operation.as_str(), native_session.as_str(), episode),
            );
            if let Err(reason) = record(shared, channel, request).await {
                return Restart::Kept(format!(
                    "자동 이어하기를 미리 기록하지 못했습니다({reason})"
                ));
            }
            Some(operation)
        }
        (None, Some(intent)) => Some(intent.operation),
        (None, None) => None,
    };
    if turn.is_some() {
        // The watcher reads this death as a stop of ours, not as a crash to hand off.
        let record = crate::services::discord::record_turn_stop_tombstone;
        record(channel, Some(&tmux), "effort_restart").await;
    }
    let refused =
        match end_channel_tmux_keeping_session(shared, provider, channel, &tmux, "effort restart")
            .await
        {
            Relaunch::Relaunched => None,
            Relaunch::Refused(reason) => Some(reason),
            Relaunch::KillFailed | Relaunch::Deferred => Some("TUI를 종료하지 못했습니다".into()),
        };
    if let Some(reason) = refused {
        // The prepared continuation of a turn that keeps running is owed nothing.
        if turn.is_some()
            && let Some(operation) = &operation
        {
            cancel(shared, channel, operation).await;
        }
        return Restart::Kept(reason);
    }
    // The ended TUI's next launch reads the settings now saved; the pendings they answer are spent.
    super::super::control::consume_reflected(shared, provider, channel).await;
    if turn.is_some() {
        let settled = settle_captured(shared, provider, turn).await;
        tracing::info!(
            channel_id = channel.get(),
            ?settled,
            "effort restart settled its turn"
        );
    }
    let Some(operation) = operation else {
        return Restart::Restarted {
            continues: false,
            told: false,
        };
    };
    match advance(shared, provider, channel, &operation).await {
        Step::Admitted { told } => Restart::Restarted {
            continues: true,
            told,
        },
        Step::Admitting { told } => Restart::Admitting { told },
        Step::Unproven(reason) => Restart::Unfinished(format!(
            "옛 TUI의 종료 지점을 아직 기록하지 못했습니다({reason}). 기록되는 대로 이어갑니다"
        )),
        Step::Cancelled => Restart::Unfinished("자동 이어하기가 취소되었습니다".into()),
        Step::Refused(reason) => Restart::Unfinished(reason),
    }
}

/// The not-ready intent a restart stores before it ends `tmux`: the cut episode's ticket and the
/// process the pane last bound.
fn prepared(
    provider: &ProviderKind,
    channel: ChannelId,
    tmux: &str,
    (operation, native_session, episode): (&str, &str, &str),
) -> Request {
    let bound = witness::bound(provider, channel.get(), tmux);
    let bind_nonce = bound.ok().flatten().map(|bound| bound.nonce().to_string());
    Request {
        operation: operation.into(),
        native_session: native_session.into(),
        interrupted_episode: Some(episode.into()),
        desired_revision: chrono::Utc::now().timestamp_millis().max(0) as u64,
        ready: false,
        cut: Some(Cut {
            tmux: tmux.into(),
            bind_nonce,
        }),
    }
}

/// Stores `request` in the channel's owner snapshot.
async fn record(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    request: Request,
) -> Result<Intent, String> {
    let generation = crate::services::discord::runtime_store::process_generation();
    let mailbox = shared.mailbox(channel);
    mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("owner not adopted")?;
            owner.prepare_effort(Some(request), channel.get(), generation)
        })
        .await
}

/// Closes `operation`'s continuation and barrier; a commit that fails leaves it to the next wake.
pub(super) async fn cancel(shared: &Arc<SharedData>, channel: ChannelId, operation: &str) {
    let operation = operation.to_string();
    let mailbox = shared.mailbox(channel);
    let cancelled = mailbox.v2_request(move |slot| {
        slot.as_mut()
            .ok_or("owner not adopted")?
            .cancel_effort(&operation)
    });
    if let Err(error) = cancelled.await {
        tracing::warn!(channel_id = channel.get(), %error, "effort intent not cancelled");
    }
}

/// Whether the channel's owner takes a synthetic start, the only path a continuation has.
async fn owner_takes(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> bool {
    use crate::services::discord::queue_io::input_owner::{ingress, origin::InputOrigin};
    ingress::execution(provider, channel.get()).owner_takes(InputOrigin::HttpStart)
        && matches!(ingress::adopt(shared, provider, channel).await, Ok(true))
}

/// The steps left once the old TUI ended: end record, ready, notice and admission. Each repeats
/// safely, so a later wake or a restarted server reruns them for the stored intent.
pub(super) async fn advance(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    operation: &str,
) -> Step {
    let Some(intent) = stored(shared, channel).await else {
        return Step::Refused("자동 이어하기 기록이 없습니다".into());
    };
    if intent.operation != operation {
        return Step::Refused("다른 재기동이 자동 이어하기를 맡았습니다".into());
    }
    if intent.cancelled {
        return Step::Cancelled;
    }
    let Some(episode) = intent.interrupted_episode.clone() else {
        return Step::Refused("중단한 턴이 없습니다".into());
    };
    if !intent.ready {
        if let Some(cut) = &intent.cut {
            let at = (channel.get(), cut.tmux.clone(), intent.operation.clone());
            if let Err(error) = end_record(provider, at).await {
                tracing::warn!(channel_id = channel.get(), %error, "effort restart end unproven");
                return Step::Unproven(error);
            }
        }
        let request = Request {
            ready: true,
            cut: None,
            ..request_of(&intent)
        };
        match record(shared, channel, request).await {
            Ok(ready) if ready.cancelled => return Step::Cancelled,
            Ok(_) => {}
            Err(reason) if reason.contains("cancelled") => return Step::Cancelled,
            Err(reason) => return Step::Refused(reason),
        }
    }
    // Staged once the end is proven, keyed by the cut turn, however often this runs.
    let told = match shared.pg_pool.as_ref() {
        Some(pool) => {
            let staged = notice::interrupted(pool, provider.as_str(), channel.get(), &episode);
            staged.await.is_ok()
        }
        None => false,
    };
    match admit(shared, provider, channel, &intent).await {
        Admission::Accepted => Step::Admitted { told },
        Admission::Pending => Step::Admitting { told },
        Admission::Rejected(reason) if reason.contains("cancelled") => Step::Cancelled,
        Admission::Rejected(reason) => Step::Refused(reason),
    }
}

fn request_of(intent: &Intent) -> Request {
    Request {
        operation: intent.operation.clone(),
        native_session: intent.native_session.clone(),
        interrupted_episode: intent.interrupted_episode.clone(),
        desired_revision: intent.desired_revision,
        ready: intent.ready,
        cut: intent.cut.clone(),
    }
}

/// Records where the ended process's transcript ends, read off the async workers. A pane that
/// bound no transcript leaves no open turn to close.
async fn end_record(
    provider: &ProviderKind,
    (channel, tmux, operation): (u64, String, String),
) -> Result<bool, String> {
    let Some(bound) = witness::bound(provider, channel, &tmux)? else {
        return Ok(false);
    };
    let root = crate::config::runtime_root().ok_or("no runtime root")?;
    let read = move || witness::record(&root, bound, &operation);
    tokio::task::spawn_blocking(read)
        .await
        .map_err(|error| error.to_string())?
}

enum Admission {
    Accepted,
    Pending,
    Rejected(String),
}

/// Admits the cut episode's one continuation at the owner's front under the ticket its intent
/// holds, so a repeat admits nothing new. An unconfirmed admission is asked again under that ticket.
async fn admit(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    intent: &Intent,
) -> Admission {
    if matches!(provider, ProviderKind::Claude) {
        // The resumed CLI must not continue the cut turn itself: the continuation submits first.
        crate::services::claude_tui::session::expect_owner_first_submit(&intent.native_session);
    }
    let start = crate::services::discord::router::message_handler::owner_admit_start;
    let turn = format!("effort:{}:{}", channel.get(), intent.operation);
    for wait_ms in [200, 800, 0] {
        match start(shared, channel, (&turn, PROMPT), intent.context()).await {
            Some(Intake::Accepted) => return Admission::Accepted,
            Some(Intake::Pending(_)) if wait_ms > 0 => {
                tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
            }
            Some(Intake::Pending(_)) => return Admission::Pending,
            Some(Intake::Rejected(reason)) => return Admission::Rejected(reason),
            None => return Admission::Rejected("owner does not take this start".into()),
        }
    }
    Admission::Pending
}
