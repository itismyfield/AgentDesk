//! Live settings changes on the channel's recorded pane, confirmed by its own fresh statusline.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use poise::serenity_prelude::ChannelId;
use serde_json::Value;

use super::{EffortLevel, SharedData};
use crate::db::dispatched_sessions::hosted_execution::{HostedLookup, HostedRecord, HostedState};
use crate::services::claude_tui::{composer_lock, hook_bundle::statusline, host_input, input};
use crate::services::provider::ProviderKind;
use crate::services::session_host::{HerdrTarget, herdr_endpoints};
use crate::services::tmux_common as tc;

const WAIT: Duration = Duration::from_millis(if cfg!(test) { 250 } else { 45_000 });

type Target = (HerdrTarget, String, String);

async fn target(shared: &SharedData, channel: ChannelId) -> Result<Target, String> {
    shared
        .check_settings_owner(&ProviderKind::Claude, channel)
        .await
        .map_err(|e| e.to_string())?;
    let (native, cwd, name) = {
        let core = shared.core.lock().await;
        let session = core.sessions.get(&channel).ok_or("live pane 세션 없음")?;
        (
            session.session_id.clone().ok_or("pane session_id 없음")?,
            session.current_path.clone().ok_or("채널 cwd 없음")?,
            session
                .channel_name
                .clone()
                .ok_or("pane logical key 없음")?,
        )
    };
    let logical = ProviderKind::Claude.build_tmux_session_name(&name);
    let pool = shared.pg_pool.as_ref().ok_or("pane 소유 기록 DB 없음")?;
    let lookup = crate::services::discord::host_key_derivation::derive_hosted_lookup(
        pool,
        std::slice::from_ref(&shared.token_hash),
        &ProviderKind::Claude,
        channel.get(),
        &logical,
    )
    .await;
    let HostedLookup::Found(found) = lookup else {
        return Err("채널 소유 pane 조회 실패".into());
    };
    let HostedRecord::Known(ref record) = found.record else {
        return Err("pane 실행 기록 미확인".into());
    };
    let owner = &record.owner;
    if record.state != HostedState::Bound
        || owner.provider != "claude"
        || owner.discord_token_hash != shared.token_hash
        || owner.channel_id != channel.get().to_string()
        || owner.logical_key != logical
    {
        return Err("채널 소유 Bound pane 불일치".into());
    }
    let role = shared
        .dispatch
        .role_overrides
        .get(&channel)
        .map_or(channel, |v| *v);
    let agent = crate::services::discord::settings::resolve_role_binding(role, Some(&name))
        .map(|b| b.role_id)
        .or(shared.settings.read().await.agent.clone());
    let metadata: (Option<String>, Option<String>, Option<String>) = sqlx::query_as("SELECT cwd, agent_id, claude_session_id FROM sessions WHERE id = $1 AND hosted_execution = $2")
        .bind(found.session_id()).bind(serde_json::to_value(record).map_err(|e| e.to_string())?).fetch_one(pool).await.map_err(|e| format!("pane 실행 메타데이터: {e}"))?;
    if metadata != (Some(cwd.clone()), agent, Some(native.clone()))
        || herdr_endpoints().working_dir(record).as_deref() != Some(std::path::Path::new(&cwd))
    {
        return Err("pane cwd·agent·session_id가 채널 현재 값과 다릅니다".into());
    }
    let pane = herdr_endpoints()
        .target(record)
        .ok_or("pane endpoint 미확인")?;
    Ok((pane, logical, native))
}

fn wait<T>(mut observe: impl FnMut() -> Result<T, String>) -> Result<T, String> {
    let deadline = Instant::now() + WAIT;
    loop {
        let reason = match observe() {
            Ok(value) => return Ok(value),
            Err(reason) => reason,
        };
        if Instant::now() >= deadline {
            return Err(reason);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn ready(capture: &str) -> bool {
    !tc::tmux_capture_indicates_claude_tui_interactive_modal(capture)
        && input::final_prompt_ready(&input::prompt_readiness_snapshot_from_capture(
            Some(capture),
            true,
        ))
}

fn record(session: &str, after: f64) -> Result<Value, &'static str> {
    let path = statusline::record_path(&statusline::record_dir(), session);
    let bytes = std::fs::read(path).map_err(|_| "기록 없음")?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "기록 형식 미확인")?;
    if value["session_id"].as_str() != Some(session) {
        return Err("다른 session_id 기록");
    }
    if !value["written_at"].as_f64().is_some_and(|at| at > after) {
        return Err("오래된 written_at 기록");
    }
    Ok(value)
}

async fn with_ready_pane<T: Send + 'static>(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    apply: impl FnOnce(&HerdrTarget, &str, f64) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs_f64();
    let transition = shared.session_transition_lock(channel);
    let _transition = transition.lock().await;
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || {
        let handle = tokio::runtime::Handle::current();
        let original = handle.block_on(target(&shared, channel))?;
        let (pane, logical, native) = original.clone();
        wait(|| {
            pane.capture(-80)
                .filter(|c| ready(c))
                .ok_or("pane ready 대기 시간 초과".into())
        })?;
        composer_lock::with_composer_mutation_lock(&logical, || {
            if handle.block_on(target(&shared, channel))? != original {
                return Err("pane 소유 실행 변경".into());
            }
            let capture = pane.capture(-80).ok_or("pane 입력 상태 미확인")?;
            if composer_lock::composer_admission(&logical, || Some(capture.clone()))
                != composer_lock::ComposerAdmission::Any
                || !ready(&capture)
                || !tc::tmux_capture_indicates_claude_tui_exact_empty_composer(&capture)
            {
                return Err("초안 보호: draft 또는 준비 상태가 바뀌었습니다".into());
            }
            let result = apply(&pane, &native, after)?;
            if handle.block_on(target(&shared, channel))? != original {
                return Err("적용 미확인: pane 소유 실행 변경".into());
            }
            Ok(result)
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

pub(super) async fn effort(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    level: EffortLevel,
) -> Result<(), String> {
    with_ready_pane(shared, channel, move |pane, native, after| {
        use input::TuiInputAction::{Enter, Left, Literal, Right};
        let send =
            |plan: &[input::TuiInputAction]| host_input::run_herdr(pane, plan, None).into_legacy();
        send(&[Literal("/effort".into()), Enter])?;
        let overlay = wait(|| {
            pane.capture(-80)
                .filter(|c| tc::tmux_capture_indicates_claude_tui_selector_open(c))
                .ok_or("effort overlay 열림 미확인".into())
        })?;
        let labels = overlay
            .rsplit_once("Effort")
            .and_then(|(_, s)| s.split_once('←'));
        if !labels.is_some_and(|(s, _)| {
            s.split(|c: char| c.is_whitespace() || "[]─".contains(c))
                .filter(|s| !s.is_empty())
                .eq(["low", "medium", "high", "xhigh", "max"])
        }) {
            return Err("effort 옵션 배치 미확인".into());
        }
        let mut plan = vec![Left; 4];
        plan.extend(std::iter::repeat_n(Right, level as usize));
        plan.push(Enter);
        send(&plan)?;
        wait(|| {
            let seen = &record(native, after).map_err(str::to_string)?["effort"]["level"];
            (seen.as_str() == Some(level.as_str()))
                .then_some(())
                .ok_or_else(|| format!("effort.level 불일치: `{seen}`"))
        })
        .map_err(|reason| format!("적용 미확인: {reason}"))
    })
    .await
}

pub(in crate::services::discord::commands) async fn fast(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    desired: Option<bool>,
) -> Result<(bool, bool), String> {
    use crate::services::provider_hosting::{
        ProviderSessionDriver, resolve_provider_session_selection_with_channel,
    };
    if resolve_provider_session_selection_with_channel(
        &ProviderKind::Claude,
        true,
        Some(channel.get()),
    )
    .driver
        != ProviderSessionDriver::TuiHosting
    {
        return Err("Herdr /fast는 live Claude TUI pane에서만 지원합니다.".into());
    }
    with_ready_pane(shared, channel, move |pane, native, after| {
        let read = |after| {
            record(native, after).map_err(str::to_string)?["fast_mode"]
                .as_bool()
                .ok_or("fast_mode 없는 기록".into())
        };
        let before = wait(|| read(after))?;
        let next = desired.unwrap_or(!before);
        if next == before {
            return Ok((next, false));
        }
        let sent_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs_f64();
        use input::TuiInputAction::{Enter, Literal};
        host_input::run_herdr(pane, &[Literal("/fast".into()), Enter], None).into_legacy()?;
        wait(|| {
            let seen = read(sent_at)?;
            (seen == next)
                .then_some(())
                .ok_or_else(|| format!("fast_mode 불일치: `{seen}`"))
        })
        .map_err(|e| format!("적용 미확인: {e}"))?;
        Ok((next, true))
    })
    .await
}
