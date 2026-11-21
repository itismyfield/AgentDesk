//! `/effort` on a Herdr Codex pane: the exact execution ends and the level is saved under the
//! same held exclusion, so the next launch, from that retired row, resumes its thread with it.
//! A cut turn is owed one continuation, as on tmux; the Claude pane takes the level in place.
use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;

use super::tmux::{self, Restart, Step};
use crate::db::dispatched_sessions::hosted_execution::HostedState;
use crate::services::codex::herdr_turn::restart_intent::{EffortResume, live_source};
use crate::services::discord::SharedData;
use crate::services::discord::herdr_terminate::http_force::{Released, terminate_for_http};
use crate::services::discord::herdr_terminate::{OperatorTerminate, RetiredExclusion};
use crate::services::discord::queue_io::input_owner::restart_gate::Gate;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::effort_intent::{Cut, Request};

type Target = super::super::restart::herdr::Target;

/// Validates, ends and saves `level` for a Herdr Codex channel, and says what that did.
pub(in crate::services::discord) async fn notice(
    shared: &Arc<SharedData>,
    token: &str,
    channel: ChannelId,
    provider: &ProviderKind,
    level: &'static str,
) -> String {
    let target = match live(shared, provider, channel).await {
        Ok(target) => target,
        Err(reason) => return format!("effort를 바꾸지 않았습니다: {reason}"),
    };
    let key = channel.get().to_string();
    let saved = shared
        .settings
        .read()
        .await
        .channel_effort_levels
        .get(&key)
        .cloned();
    if saved.as_deref() == Some(level) {
        return format!("effort는 이미 `{level}`로 저장돼 있습니다. 바꾼 설정이 없습니다.");
    }
    let Some(target) = target else {
        // Nothing runs to take the old options: the next launch reads the saved level.
        let save = super::super::config::update_channel_effort;
        return match save(shared, token, channel, provider, level).await {
            Ok(_) => format!(
                "effort를 `{level}`로 저장했습니다. 실행 중인 TUI가 없어 다음 기동부터 적용합니다."
            ),
            Err(error) => format!("effort 설정을 저장하지 못했습니다: {error}"),
        };
    };
    let save = Save {
        shared: shared.clone(),
        token: token.to_string(),
        provider: provider.clone(),
        channel,
        level,
    };
    let (restart, stored) = restart(shared, provider, channel, (*target, save)).await;
    let said = format!("effort를 `{level}`로 저장했습니다.");
    match (restart, stored) {
        (Restart::Kept(reason), _) => {
            format!("effort를 바꾸지 않았습니다: 지금은 재기동하지 못했습니다({reason}).")
        }
        (restart, Some(Err(error))) => format!(
            "기존 TUI는 종료했지만 effort 설정을 저장하지 못했습니다: {error}. 같은 세션은 이전 값으로 이어집니다.{}",
            match restart {
                Restart::Restarted {
                    continues: true, ..
                }
                | Restart::Admitting { .. } => {
                    " 중단한 작업은 자동으로 이어갑니다."
                }
                _ => "",
            }
        ),
        (restart, _) => super::super::tui_passthrough::restart_notice(&said, provider, restart),
    }
}

/// The channel's execution to end: `None` when none runs, refused while one is still starting.
async fn live(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<Option<Box<Target>>, String> {
    use super::super::restart::herdr::{Selected, select};
    let target = match select(shared, provider, channel).await {
        Selected::Vacant => return Ok(None),
        Selected::Refused(_) => return Err("현재 Herdr 세션을 안전하게 확인할 수 없습니다".into()),
        Selected::Target(target) => target,
    };
    match target.record.state {
        HostedState::Retired => Ok(None),
        HostedState::Bound => Ok(Some(target)),
        // Its thread is not bound yet, so nothing names the session a restart would resume.
        _ => Err("세션이 아직 시작 중입니다. 시작이 끝난 뒤 다시 골라 주세요".into()),
    }
}

/// The save that runs only once the exact execution is retired, under its held exclusion.
struct Save {
    shared: Arc<SharedData>,
    token: String,
    provider: ProviderKind,
    channel: ChannelId,
    level: &'static str,
}

struct Saved(Result<(), String>);
impl Released for Saved {
    fn released(self) -> BoxFuture<'static, Self> {
        Box::pin(async move { self })
    }
}

/// Runs once the exact execution is retired: the provider session stays for the next launch to
/// resume, and the level is saved before any launch can read it.
async fn postlude(held: &RetiredExclusion, pool: sqlx::PgPool, save: Save) -> Saved {
    if let Err(error) = held.disconnect_session_keeping_provider(&pool).await {
        tracing::warn!(%error, "effort restart disconnect failed");
    }
    let Save {
        shared,
        token,
        provider,
        channel,
        level,
    } = save;
    let update = super::super::config::update_channel_effort;
    let saved = update(&shared, &token, channel, &provider, level).await;
    Saved(saved.map(drop).map_err(|error| error.to_string()))
}

/// Ends `target` and saves the level in its retire postlude. The second value is that save's
/// result, absent when nothing was ended.
async fn restart(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    (target, save): (Target, Save),
) -> (Restart, Option<Result<(), String>>) {
    // Owner delivery waits for this restart; its release wakes the owner.
    let Ok(gate) = Gate::begin(shared, channel) else {
        return (
            Restart::Kept("다른 재시작 요청을 처리하고 있습니다".into()),
            None,
        );
    };
    let kept = |reason: String| (Restart::Kept(reason), None);
    let expect = crate::services::discord::herdr_terminate::restart_recovery::expected_for_restart;
    let expected = match expect(shared, &target.record, target.dispatch.clone()).await {
        Ok(expected) => expected,
        Err(result) => {
            return kept(format!(
                "기존 실행을 확인하지 못했습니다({})",
                result.outcome().1
            ));
        }
    };
    let native = match live_source(channel.get()) {
        Ok((_, source)) => source.session_id,
        Err(reason) => return kept(format!("이어갈 세션을 특정하지 못했습니다({reason})")),
    };
    let capture = crate::services::discord::herdr_terminate::capture_turn;
    let turn = match capture(shared, provider, channel).await {
        Ok(turn) => turn,
        Err(reason) => return kept(format!("진행 중인 턴을 특정하지 못했습니다({reason})")),
    };
    if tmux::stored(shared, channel)
        .await
        .is_some_and(|intent| intent.pending().is_some())
    {
        return kept("앞선 재기동의 자동 이어하기가 아직 시작되지 않았습니다".into());
    }
    if turn.is_some() && !tmux::owner_takes(shared, provider, channel).await {
        return kept("이 채널의 입력 경로는 진행 중 턴의 즉시 재기동을 지원하지 않습니다".into());
    }
    let episode = match &turn {
        Some(turn) => match turn.nonce() {
            Some(nonce) => Some(nonce.to_string()),
            None => return kept("진행 중인 턴의 식별자를 확인하지 못했습니다".into()),
        },
        None => None,
    };
    // Stored before anything ends: the retired row it names is the one the next launch resumes.
    let operation = uuid::Uuid::new_v4().simple().to_string();
    let logical = target.record.owner.logical_key.clone();
    let bound = super::witness::bound(provider, channel.get(), &logical);
    let request = Request {
        operation: operation.clone(),
        native_session: native,
        interrupted_episode: episode,
        desired_revision: chrono::Utc::now().timestamp_millis().max(0) as u64,
        ready: false,
        cut: Some(Cut {
            tmux: logical,
            bind_nonce: bound.ok().flatten().map(|bound| bound.nonce().to_string()),
            herdr: Some(target.record.execution_nonce.clone()),
        }),
    };
    if let Err(reason) = tmux::record(shared, channel, request).await {
        return kept(format!(
            "자동 이어하기를 미리 기록하지 못했습니다({reason})"
        ));
    }
    let Some(registry) = shared.health_registry() else {
        tmux::cancel(shared, channel, &operation).await;
        return kept("종료 경로를 사용할 수 없습니다".into());
    };
    let request = OperatorTerminate {
        session_key: target.key.clone(),
        execution_nonce: target.record.execution_nonce.clone(),
    };
    let pool = target.pool.clone();
    let ended = (request, expected);
    let (result, saved) = terminate_for_http(
        &registry,
        target.pool,
        ended,
        "effort_restart",
        move |held| postlude(held, pool, save).boxed(),
    )
    .await;
    let Some(Saved(saved)) = saved else {
        // The execution keeps running: its turn owes nothing and the level stays unsaved.
        tmux::cancel(shared, channel, &operation).await;
        return kept(format!(
            "기존 실행을 종료하지 못했습니다({})",
            result.outcome().1
        ));
    };
    drop(gate);
    if turn.is_none() {
        let done = Restart::Restarted {
            continues: false,
            told: false,
        };
        return (done, Some(saved));
    }
    let restart = match tmux::advance(shared, provider, channel, &operation).await {
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
    };
    (restart, Some(saved))
}

/// Whether the Herdr execution `nonce` a restart named is retired: `Some(false)` while it, or a
/// row the restart cannot tell from it, still runs; `None` when the row is unreadable.
pub(super) async fn ended(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    nonce: &str,
) -> Option<bool> {
    use super::super::restart::herdr::{Selected, select};
    match select(shared, provider, channel).await {
        Selected::Target(target) => Some(
            target.record.execution_nonce == nonce && target.record.state == HostedState::Retired,
        ),
        Selected::Vacant => Some(false),
        Selected::Refused(_) => None,
    }
}

/// The thread an effort restart owes the channel's next Herdr Codex launch, read from its
/// owner's stored intent.
pub(in crate::services::discord) async fn herdr_resume(
    shared: &Arc<SharedData>,
    channel: ChannelId,
) -> Option<EffortResume> {
    let intent = tmux::stored(shared, channel).await?;
    let retired_nonce = intent.cut.as_ref()?.herdr.clone()?;
    (!intent.cancelled).then(|| EffortResume {
        required: intent.pending().is_some(),
        operation: intent.operation,
        native_session: intent.native_session,
        retired_nonce,
    })
}
