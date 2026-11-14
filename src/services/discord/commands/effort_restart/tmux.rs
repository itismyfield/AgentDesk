//! The restart itself, under the channel's transition guard from capture to admission.
use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::{notice, witness};
use crate::services::discord::SharedData;
use crate::services::discord::queue_io::input_owner::ingress::Intake;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::PendingQueueItem;
use crate::services::turn_orchestrator::effort_intent::{EffortRestart, PROMPT, Request};

/// What `/effort` did to the channel's TUI after its level was saved.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum Restart {
    /// The TUI ended. `continues`: a turn was cut and its continuation is admitted, so the owner
    /// resumes the session now, else the next input does. `told`: that turn's notice is staged.
    Restarted { continues: bool, told: bool },
    /// No TUI runs: the next launch takes the level.
    NotRunning,
    /// Nothing was ended, for this reason.
    Kept(String),
    /// The TUI ended and its turn was cut, but the continuation is not admitted.
    Unfinished(String),
}

/// A restart's cut turn: the session it resumes and the episode its continuation answers for.
pub(super) struct Owed {
    pub(super) operation: String,
    pub(super) native_session: String,
    pub(super) episode: String,
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
    if turn.is_some() {
        // The watcher reads this death as a stop of ours, not as a crash to hand off.
        let record = crate::services::discord::record_turn_stop_tombstone;
        record(channel, Some(&tmux), "effort_restart").await;
    }
    match end_channel_tmux_keeping_session(shared, provider, channel, &tmux, "effort restart").await
    {
        Relaunch::Relaunched => {}
        Relaunch::Refused(reason) => return Restart::Kept(reason),
        Relaunch::KillFailed | Relaunch::Deferred => {
            return Restart::Kept("TUI를 종료하지 못했습니다".into());
        }
    }
    // The level is applied by this end; the next turn has no relaunch left to do for it.
    super::super::control::release_relaunch_pending(shared, channel).await;
    let cut = turn.as_ref().map(|turn| turn.nonce().map(str::to_string));
    if turn.is_some() {
        let settled = settle_captured(shared, provider, turn).await;
        tracing::info!(
            channel_id = channel.get(),
            ?settled,
            "effort restart settled its turn"
        );
    }
    let owed = match (cut, waiting) {
        (Some(Some(episode)), _) => {
            let operation = uuid::Uuid::new_v4().simple().to_string();
            let at = (channel.get(), tmux.clone(), operation.clone());
            // Unproven, the continuation is still owed; its start waits on the pane's own evidence.
            if let Err(error) = end_record(provider, at).await {
                tracing::warn!(channel_id = channel.get(), %error, "effort restart end unproven");
            }
            Owed {
                operation,
                native_session,
                episode,
            }
        }
        (Some(None), _) => {
            return Restart::Unfinished(
                "중단한 턴의 식별자가 없어 이어하기를 만들지 못했습니다".into(),
            );
        }
        (None, Some(intent)) => Owed {
            operation: intent.operation,
            native_session,
            episode: intent.interrupted_episode.unwrap_or_default(),
        },
        (None, None) => {
            return Restart::Restarted {
                continues: false,
                told: false,
            };
        }
    };
    let episode = owed.episode.clone();
    if let Err(reason) = admit(shared, provider, channel, owed).await {
        return Restart::Unfinished(reason);
    }
    // Staged only once the continuation it promises is admitted.
    let told = match shared.pg_pool.as_ref() {
        Some(pool) => {
            let staged = notice::interrupted(pool, provider.as_str(), channel.get(), &episode);
            staged.await.is_ok()
        }
        None => false,
    };
    Restart::Restarted {
        continues: true,
        told,
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

/// Reads the ended process's transcript off the async workers.
async fn end_record(
    provider: &ProviderKind,
    (channel, tmux, operation): (u64, String, String),
) -> Result<bool, String> {
    let root = crate::config::runtime_root().ok_or("no runtime root")?;
    let bound = witness::bound(provider, channel, &tmux)?;
    let read = move || witness::record(&root, bound, &operation);
    tokio::task::spawn_blocking(read)
        .await
        .map_err(|error| error.to_string())?
}

/// Records the cut episode's one continuation as ready and admits it at the owner's front, under
/// the ticket the episode derives, so a repeat admits nothing new.
pub(super) async fn admit(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    owed: Owed,
) -> Result<(), String> {
    let generation = crate::services::discord::runtime_store::process_generation();
    let request = Request {
        operation: owed.operation,
        native_session: owed.native_session.clone(),
        interrupted_episode: Some(owed.episode),
        desired_revision: chrono::Utc::now().timestamp_millis().max(0) as u64,
        ready: true,
    };
    let intent = shared
        .mailbox(channel)
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("owner not adopted")?;
            owner.prepare_effort(Some(request), channel.get(), generation)
        })
        .await?;
    if matches!(provider, ProviderKind::Claude) {
        // The resumed CLI must not continue the cut turn itself: the continuation submits first.
        crate::services::claude_tui::session::expect_owner_first_submit(&owed.native_session);
    }
    let admit = crate::services::discord::router::message_handler::owner_admit_start;
    let turn = format!("effort:{}:{}", channel.get(), intent.operation);
    match admit(shared, channel, (&turn, PROMPT), intent.context()).await {
        Some(Intake::Accepted | Intake::Pending(_)) => Ok(()),
        Some(Intake::Rejected(reason)) => Err(reason),
        None => Err("owner does not take this start".into()),
    }
}
