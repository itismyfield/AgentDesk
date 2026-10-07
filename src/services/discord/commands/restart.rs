use std::sync::Arc;

use poise::serenity_prelude as serenity;

use crate::services::discord::admin_host_guard::ManagedReset;
use crate::services::provider::ProviderKind;

use super::super::{
    Context, Error, SharedData, check_auth, mailbox_cancel_active_turn, mailbox_has_active_turn,
};

const RESTART_SEED_PROMPT: &str = "안녕하세요. 세션 재시작 완료.";

/// Outcome of resolving the reload action against the current session state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RestartAction {
    /// No active session — the seed turn will create one when a workspace is resolvable.
    NoActiveSession,
    /// Active session, no captured provider session_id (fresh session, hasn't completed
    /// its first turn yet). Killing tmux is safe but resume isn't possible.
    ActiveSessionWithoutSessionId,
    /// Active session with a captured provider session_id. Providers with resume support
    /// receive it on the immediate seed turn.
    ActiveSessionResumable { session_id: String },
}

/// Inspect the channel's current session state and decide what kind of reload action
/// applies. Pure with respect to the captured snapshot, so it can be unit-tested by
/// passing the (`session_present`, `session_id`) tuple directly via the helper below.
pub(super) async fn resolve_restart_action(
    shared: &SharedData,
    channel_id: poise::serenity_prelude::ChannelId,
) -> RestartAction {
    let snapshot = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .map(|session| session.session_id.clone())
    };
    classify_restart_action(snapshot)
}

/// Pure classifier — exposed for tests.
pub(super) fn classify_restart_action(session_snapshot: Option<Option<String>>) -> RestartAction {
    match session_snapshot {
        None => RestartAction::NoActiveSession,
        Some(None) => RestartAction::ActiveSessionWithoutSessionId,
        Some(Some(sid)) if sid.trim().is_empty() => RestartAction::ActiveSessionWithoutSessionId,
        Some(Some(sid)) => RestartAction::ActiveSessionResumable { session_id: sid },
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RestartSeedStatus {
    Started,
    Busy,
    Failed(String),
}

fn provider_supports_resume(provider: &ProviderKind) -> bool {
    provider
        .capabilities()
        .is_some_and(|capabilities| capabilities.supports_resume)
}

fn restart_resume_note(provider: &ProviderKind, action: &RestartAction) -> String {
    match action {
        RestartAction::ActiveSessionResumable { session_id }
            if provider_supports_resume(provider) =>
        {
            format!("대화 컨텍스트는 session_id `{session_id}`에서 `--resume`으로 이어집니다.")
        }
        RestartAction::ActiveSessionResumable { .. } => {
            "이 provider는 resume을 지원하지 않아 신규 세션으로 시작합니다.".to_string()
        }
        RestartAction::ActiveSessionWithoutSessionId => {
            "기존 session_id가 없어 신규 세션으로 시작합니다.".to_string()
        }
        RestartAction::NoActiveSession => {
            "활성 세션이 없어 가능한 경우 새 세션으로 시작합니다.".to_string()
        }
    }
}

fn restart_tmux_note(tmux_name: Option<&str>) -> String {
    tmux_name
        .map(|name| format!("tmux reset 요청됨 (`{name}`)"))
        .unwrap_or_else(|| "관리 중인 tmux 세션 없음 — 새 세션 시작 시 자동 생성".to_string())
}

fn restart_seed_note(status: &RestartSeedStatus) -> String {
    match status {
        // Successful seed turn needs no narration — the agent's own greeting
        // turn follows immediately and speaks for itself.
        RestartSeedStatus::Started => String::new(),
        RestartSeedStatus::Busy => {
            "기존 턴 정리가 아직 끝나지 않아 인사말 turn은 시작하지 못했습니다.".to_string()
        }
        RestartSeedStatus::Failed(error) => {
            format!("인사말 turn 시작 실패: {error}")
        }
    }
}

fn build_restart_response(
    provider: &ProviderKind,
    action: &RestartAction,
    tmux_name: Option<&str>,
    seed_status: &RestartSeedStatus,
) -> String {
    let header = format!(
        "♻ 세션 재시작됨 (provider={}). 현재 인증된 MCP/설정이 부착됐습니다.",
        provider.as_str()
    );
    let parts = [
        header,
        restart_tmux_note(tmux_name),
        restart_resume_note(provider, action),
        restart_seed_note(seed_status),
    ];
    parts
        .iter()
        .filter(|line| !line.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

async fn start_restart_seed_turn(ctx: &Context<'_>) -> RestartSeedStatus {
    const MAX_ATTEMPTS: usize = 30;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

    let channel_id = ctx.channel_id();
    let channel_name_hint = {
        let data = ctx.data().shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
    };
    let metadata = serde_json::json!({
        "command": "/restart",
        "provider": ctx.data().provider.as_str(),
    });

    for attempt in 0..MAX_ATTEMPTS {
        match super::super::router::start_headless_turn(
            (
                ctx.serenity_context(),
                &ctx.data().shared,
                &ctx.data().token,
            ),
            channel_id,
            RESTART_SEED_PROMPT,
            "/restart",
            Some("/restart"),
            Some(metadata.clone()),
            channel_name_hint.clone(),
        )
        .await
        {
            Ok(_) => return RestartSeedStatus::Started,
            Err(super::super::router::HeadlessTurnStartError::Conflict(_))
                if attempt + 1 < MAX_ATTEMPTS =>
            {
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Err(super::super::router::HeadlessTurnStartError::Conflict(_)) => {
                return RestartSeedStatus::Busy;
            }
            Err(
                super::super::router::HeadlessTurnStartError::Internal(error)
                | super::super::router::HeadlessTurnStartError::InvalidTarget(error),
            ) => {
                return RestartSeedStatus::Failed(error);
            }
        }
    }

    RestartSeedStatus::Busy
}

/// Cancels the in-flight turn and resets the managed session, after the host check: `Err`
/// carries why a session the guard keeps was left as it was, turn and process included.
async fn restart_managed_session<Warn, WarnFut>(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    command_name: &'static str,
    warn_in_flight: Warn,
) -> Result<Result<Option<String>, String>, Error>
where
    Warn: FnOnce() -> WarnFut,
    WarnFut: std::future::Future<Output = Result<(), Error>>,
{
    // Refused before the in-flight turn is cancelled: the session is not a legacy tmux one.
    let refusal = super::super::admin_host_guard::managed_reset_refusal;
    if let Some(reason) = refusal(shared, provider, channel_id, true, true, None).await {
        return Ok(Err(reason));
    }

    // Warn if a turn is in flight, then cancel it via the same path /stop uses.
    if mailbox_has_active_turn(shared, channel_id).await {
        warn_in_flight().await?;
        let cancel = mailbox_cancel_active_turn(shared, channel_id).await;
        if let Some(token) = cancel.token {
            super::super::turn_bridge::stop_active_turn(
                provider,
                &token,
                super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession,
                command_name,
            )
            .await;
        }
    }

    // Kill the managed tmux/process session without clearing session_id when the
    // provider can resume. The seed turn below immediately respawns the provider.
    let reset = super::control::reset_channel_provider_state(
        http,
        shared,
        provider,
        channel_id,
        command_name,
        !provider_supports_resume(provider),
        false, // do NOT clear history
        true,  // recreate (kill) the tmux session so the seed turn fully respawns provider
    )
    .await;
    Ok(match reset {
        ManagedReset::Applied(tmux_name) => Ok(tmux_name),
        ManagedReset::Refused(reason) => Err(reason),
    })
}

async fn run_restart(ctx: Context<'_>, command_name: &'static str) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }
    if !super::enforce_slash_command_policy(&ctx, command_name).await? {
        return Ok(());
    }

    log_command_received!(ctx.channel_id().get(), user_name, command_name);
    ctx.say("♻ 세션 재시작 중...").await?;

    let channel_id = ctx.channel_id();
    let action = resolve_restart_action(&ctx.data().shared, channel_id).await;
    let http = ctx.serenity_context().http.clone();
    let (shared, provider) = (&ctx.data().shared, &ctx.data().provider);
    let warn = || async {
        ctx.say("⚠ 진행 중 턴 1회 손실 가능 — 안전하게 중단합니다.")
            .await
            .map(|_| ())
            .map_err(Error::from)
    };
    let restarted =
        restart_managed_session(&http, shared, provider, channel_id, command_name, warn);
    let tmux_name = match restarted.await? {
        Ok(tmux_name) => tmux_name,
        Err(reason) => {
            ctx.say(format!("♻ 세션을 재시작하지 않았어요: {reason}"))
                .await?;
            return Ok(());
        }
    };

    let seed_status = start_restart_seed_turn(&ctx).await;
    ctx.say(build_restart_response(
        &ctx.data().provider,
        &action,
        tmux_name.as_deref(),
        &seed_status,
    ))
    .await?;
    log_info_event!(
        "discord_restart_triggered",
        channel_id = channel_id.get(),
        user_name = %user_name,
        command = %command_name,
        provider = ctx.data().provider.as_str(),
        status = ?seed_status,
    );
    Ok(())
}

/// Restart the current provider session immediately.
#[poise::command(slash_command, rename = "restart")]
pub(in crate::services::discord) async fn cmd_restart(ctx: Context<'_>) -> Result<(), Error> {
    run_restart(ctx, "/restart").await
}

#[cfg(test)]
#[cfg(unix)]
mod host_guard_tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::services::discord::admin_host_guard::tests::process;
    use crate::services::discord::host_defer_gate::tests::{
        Case, ScriptedTmux, map_channel, postgres,
    };
    use crate::services::discord::host_teardown_gate::test_support::{
        busy_turn, channel_key, shared_on, turn_kept,
    };

    // `/restart` on a session the host guard keeps, or on a Herdr-configured channel, leaves its
    // turn, process and tmux as they are and says why; otherwise it cancels and kills as in main.
    #[tokio::test]
    async fn restart_keeps_the_turn_and_process_of_a_refused_session_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let tmux = ScriptedTmux::install();
        let (db, pool) = postgres().await;
        let shared = shared_on(&pool).await;
        let (provider, http) = (ProviderKind::Claude, Arc::new(serenity::Http::new("")));
        for (n, case) in Case::ALL.into_iter().enumerate() {
            let channel = serenity::ChannelId::new(1_479_671_302_387_071_000 + n as u64);
            let channel_name = format!("p4c2-restart-{n}");
            let name = provider.build_tmux_session_name(&channel_name);
            map_channel(&shared, channel, &channel_name).await;
            case.seed(&pool, &channel_key(&shared, &name), &name, channel.get())
                .await;
            let token = busy_turn(&shared, channel, &name).await;
            let alive = process(&name, n as u32 + 68_000);
            tmux.take_calls();

            let warned = AtomicBool::new(false);
            let warn = || {
                warned.store(true, Ordering::SeqCst);
                async { Ok(()) }
            };
            let restart =
                restart_managed_session(&http, &shared, &provider, channel, "/restart", warn);
            let restarted = restart.await.expect("restart");
            let warned = warned.load(Ordering::SeqCst);
            if case.admitted() {
                assert!(restarted.is_ok(), "{case:?}: {restarted:?}");
                assert!(warned, "{case:?}: main warns of the in-flight turn");
                let kept = turn_kept(&shared, channel, &token).await;
                assert!(!kept, "{case:?}: main cancels the turn");
                let calls = tmux.take_calls();
                let recreated = calls.iter().any(|call| call.contains(&name));
                assert!(recreated, "{case:?}: main recreates tmux: {calls:?}");
                crate::services::session_backend::remove_process_session(&name);
                continue;
            }
            let reason = restarted.expect_err("a kept session refuses the restart");
            assert!(reason.contains(&name), "{case:?}: {reason}");
            assert!(!warned, "{case:?}: no in-flight warning");
            assert!(
                turn_kept(&shared, channel, &token).await,
                "{case:?}: turn kept"
            );
            assert!(alive.load(Ordering::SeqCst), "{case:?}: process kept");
            assert_eq!(tmux.take_calls(), Vec::<String>::new(), "{case:?}: no tmux");
            crate::services::session_backend::remove_process_session(&name);
        }

        // A legacy row the guard admits, on a configured channel with admission stopped.
        use crate::services::discord::host_teardown_gate::test_support::Stored;
        use crate::services::herdr_admission::{Admission, force_for_test};
        let channel = serenity::ChannelId::new(1_479_671_302_387_071_900);
        let name = provider.build_tmux_session_name("p9b1-restart");
        map_channel(&shared, channel, "p9b1-restart").await;
        Case::Stored(Stored::Legacy)
            .seed(&pool, &channel_key(&shared, &name), &name, channel.get())
            .await;
        let token = busy_turn(&shared, channel, &name).await;
        let alive = process(&name, 68_900);
        let configured = [(channel.get(), "mac-mini")];
        let _hosts = crate::config::session_hosts::force_for_test(Some("mac-mini"), &configured);
        let _off = force_for_test(Admission::new(Some("off".as_ref()), None));
        tmux.take_calls();
        let warned = AtomicBool::new(false);
        let warn = || {
            warned.store(true, Ordering::SeqCst);
            async { Ok(()) }
        };
        let restart = restart_managed_session(&http, &shared, &provider, channel, "/restart", warn);
        let restarted = restart.await.expect("restart");
        assert!(
            turn_kept(&shared, channel, &token).await,
            "configured: turn kept"
        );
        assert!(alive.load(Ordering::SeqCst), "configured: process kept");
        assert_eq!(
            tmux.take_calls(),
            Vec::<String>::new(),
            "configured: no tmux"
        );
        assert!(
            !warned.load(Ordering::SeqCst),
            "configured: no in-flight warning"
        );
        let reason = restarted.expect_err("a configured channel refuses the restart");
        assert!(reason.contains("Herdr"), "{reason}");
        crate::services::session_backend::remove_process_session(&name);
        db.drop().await;
    }
}
