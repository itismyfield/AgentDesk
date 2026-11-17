//! Live settings changes on the channel's recorded pane, confirmed by its own fresh statusline.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use poise::serenity_prelude::ChannelId;
use serde_json::Value;

use super::SharedData;
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

/// The pane's last 80 rows, or why Herdr's reply was refused.
fn screen(pane: &HerdrTarget) -> Result<String, String> {
    pane.capture_checked(-80)
        .map_err(|why| format!("pane 읽기 실패: {why:?}"))
}

/// Ok once the pane shows a ready composer under no dialog.
fn ready_pane(pane: &HerdrTarget) -> Result<(), String> {
    ready(&screen(pane)?)
        .then_some(())
        .ok_or("pane ready 대기 시간 초과".into())
}

fn ready(capture: &str) -> bool {
    !tc::tmux_capture_indicates_claude_tui_interactive_modal(capture)
        && input::final_prompt_ready(&input::prompt_readiness_snapshot_from_capture(
            Some(capture),
            true,
        ))
}

/// One Escape closes a dialog left open, which would take the next prompt's Enter as its answer;
/// it is sent only while `open` sees it, so an idle composer never gets a stray one.
fn close_after(pane: &HerdrTarget, reason: String, open: fn(&str) -> bool, what: &str) -> String {
    let closed = if screen(pane).map_or(true, |c| open(&c)) {
        host_input::run_herdr(pane, &[input::TuiInputAction::Escape], None).into_legacy()
    } else {
        Ok(())
    };
    match closed.and_then(|()| wait(|| ready_pane(pane))) {
        Ok(()) => reason,
        Err(why) => format!("{reason} ({what}이 남았을 수 있음: {why})"),
    }
}

/// The selector's one option row, the only row led by a lowercase word, lists exactly the five
/// levels; hint text around it (`Tab to toggle`, `The default effort …`) starts uppercase.
fn effort_labels_known(overlay: &str) -> bool {
    let Some((region, _)) = overlay
        .rsplit_once("Effort")
        .and_then(|(_, s)| s.split_once('←'))
    else {
        return false;
    };
    let mut rows = region
        .lines()
        .map(|line| {
            line.split(|c: char| c.is_whitespace() || "[]─▲".contains(c))
                .filter(|s| !s.is_empty())
                .take_while(|w| w.starts_with(|c: char| c.is_ascii_lowercase()))
                .collect::<Vec<_>>()
        })
        .filter(|row| !row.is_empty());
    rows.next() == Some(vec!["low", "medium", "high", "xhigh", "max"]) && rows.next().is_none()
}

/// The open `/fast` overlay's shown state; `Some(None)` when it shows neither ON nor OFF.
fn fast_overlay(capture: &str) -> Option<Option<bool>> {
    let (overlay, _) = capture.rsplit_once("Space to toggle")?;
    let line = overlay
        .lines()
        .rev()
        .take(8)
        .find(|l| l.contains("Fast mode"))?;
    Some(
        match line.split_once("Fast mode")?.1.split_whitespace().next() {
            Some("ON") => Some(true),
            Some("OFF") => Some(false),
            _ => None,
        },
    )
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

/// Applies on a ready pane, then saves the applied value into the channel's settings while the
/// transition lock still orders commands, so a later command's value is never overwritten.
async fn with_ready_pane<T: Send + 'static>(
    shared: &Arc<SharedData>,
    (token, channel): (&str, ChannelId),
    apply: impl FnOnce(&HerdrTarget, &str, f64) -> Result<T, String> + Send + 'static,
    save: impl FnOnce(&T, &mut crate::services::discord::DiscordBotSettings),
) -> Result<T, String> {
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs_f64();
    let transition = shared.session_transition_lock(channel);
    let _transition = transition.lock().await;
    let reader = shared.clone();
    let applied = tokio::task::spawn_blocking(move || -> Result<T, String> {
        let shared = reader;
        let handle = tokio::runtime::Handle::current();
        let original = handle.block_on(target(&shared, channel))?;
        let (pane, logical, native) = original.clone();
        wait(|| ready_pane(&pane))?;
        composer_lock::with_composer_mutation_lock(&logical, || {
            if handle.block_on(target(&shared, channel))? != original {
                return Err("pane 소유 실행 변경".into());
            }
            let capture = screen(&pane)?;
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
    .map_err(|e| e.to_string())??;
    // No relaunch is queued: the live pane already runs this value.
    let mut settings = shared.settings.write().await;
    let mut candidate = settings.clone();
    save(&applied, &mut candidate);
    candidate
        .try_save_bot_settings(token)
        .map_err(|e| format!("pane에는 적용했지만 채널 설정 저장 실패: {e}"))?;
    *settings = candidate;
    Ok(applied)
}

pub(super) async fn effort(
    shared: &Arc<SharedData>,
    token: &str,
    channel: ChannelId,
    level: &'static str,
) -> Result<(), String> {
    // The overlay lists these left to right.
    let steps = ["low", "medium", "high", "xhigh", "max"]
        .into_iter()
        .position(|listed| listed == level)
        .ok_or("Herdr effort 선택창에 없는 값입니다")?;
    let apply = move |pane: &HerdrTarget, native: &str, after| {
        use input::TuiInputAction::{Enter, Left, Literal, Right};
        let send =
            |plan: &[input::TuiInputAction]| host_input::run_herdr(pane, plan, None).into_legacy();
        send(&[Literal("/effort".into()), Enter])?;
        let overlay = wait(|| {
            let capture = screen(pane)?;
            tc::tmux_capture_indicates_claude_tui_selector_open(&capture)
                .then_some(capture)
                .ok_or("effort overlay 열림 미확인".into())
        })?;
        let choose = || {
            if !effort_labels_known(&overlay) {
                return Err("effort 옵션 배치 미확인".to_string());
            }
            let mut plan = vec![Left; 4];
            plan.extend(std::iter::repeat_n(Right, steps));
            plan.push(Enter);
            send(&plan)?;
            wait(|| {
                let seen = &record(native, after).map_err(str::to_string)?["effort"]["level"];
                (seen.as_str() == Some(level))
                    .then_some(())
                    .ok_or_else(|| format!("effort.level 불일치: `{seen}`"))
            })
            .map_err(|reason| format!("적용 미확인: {reason}"))
        };
        choose().map_err(|reason| {
            let open = tc::tmux_capture_indicates_claude_tui_selector_open;
            close_after(pane, reason, open, "effort 선택창")
        })
    };
    let key = channel.get().to_string();
    with_ready_pane(shared, (token, channel), apply, |(), settings| {
        settings
            .channel_effort_levels
            .insert(key, level.to_string());
    })
    .await
}

pub(in crate::services::discord::commands) async fn fast(
    shared: &Arc<SharedData>,
    token: &str,
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
    let apply = move |pane: &HerdrTarget, native: &str, after| {
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
        let send =
            |plan: &[input::TuiInputAction]| host_input::run_herdr(pane, plan, None).into_legacy();
        let applied = || {
            let seen = read(sent_at)?;
            (seen == next)
                .then_some(())
                .ok_or_else(|| format!("fast_mode 불일치: `{seen}`"))
        };
        // Older Claude toggles at once; 2.1.319 opens an overlay that applies on Space, Enter.
        let toggle = || {
            send(&[Literal("/fast".into()), Enter])?;
            let opened = wait(|| match screen(pane).ok().and_then(|c| fast_overlay(&c)) {
                Some(Some(shown)) => Ok(Some(shown)),
                Some(None) => Err("fast 선택창 상태 미확인".into()),
                None => applied().map(|()| None),
            })
            .map_err(|e| format!("적용 미확인: {e}"))?;
            match opened {
                None => Ok(()),
                // Toggles only when the overlay agrees with the record; a typed space is Space's byte.
                Some(shown) if shown == before => {
                    send(&[Literal(" ".into()), Enter])?;
                    wait(applied).map_err(|e| format!("적용 미확인: {e}"))
                }
                Some(_) => Err("적용 미확인: fast 선택창 상태가 기록과 다름".into()),
            }
        };
        toggle().map_err(|reason| {
            close_after(pane, reason, |c| fast_overlay(c).is_some(), "fast 선택창")
        })?;
        Ok((next, true))
    };
    let mirror = &shared.overrides.fast_mode_channels;
    with_ready_pane(shared, (token, channel), apply, |&(next, _), settings| {
        settings
            .channel_fast_modes
            .insert(channel.get().to_string(), next);
        if next {
            mirror.insert(channel);
        } else {
            mirror.remove(&channel);
        }
    })
    .await
}
