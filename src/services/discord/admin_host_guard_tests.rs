//! Admin commands and diagnostics on every stored host case, through their real entries.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use poise::serenity_prelude::{ChannelId, Http};

use crate::services::discord::commands::{
    SoftClearNotifyMode, build_health_report, build_status_report, clear_channel_session_state,
    reset_channel_provider_state, reset_provider_session_if_pending,
};
use crate::services::discord::host_defer_gate::tests::{Case, ScriptedTmux, map_channel, postgres};
use crate::services::discord::host_teardown_gate::test_support::{channel_key, shared_on};
use crate::services::provider::ProviderKind;
use crate::services::session_backend::{SessionHandle, insert_process_session};

async fn session_id(shared: &crate::services::discord::SharedData, channel: ChannelId) -> bool {
    let core = shared.core.lock().await;
    core.sessions[&channel].session_id.is_some()
}

// `/clear` and a provider reset refuse a session the host guard keeps before they change
// anything; a legacy row, or no row yet, clears and kills as in main.
#[tokio::test]
async fn clear_and_reset_refuse_a_kept_session_before_any_change_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let (provider, http) = (ProviderKind::Claude, Arc::new(Http::new("")));
    for (n, case) in Case::ALL.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_302_387_061_000 + n as u64);
        let channel_name = format!("p4c2-clear-{n}");
        let name = provider.build_tmux_session_name(&channel_name);
        map_channel(&shared, channel, &channel_name).await;
        let mut core = shared.core.lock().await;
        core.sessions.get_mut(&channel).unwrap().session_id = Some("sid".into());
        drop(core);
        case.seed(&pool, &channel_key(&shared, &name), &name, channel.get())
            .await;
        let alive = Arc::new(AtomicBool::new(true));
        let (pid, process) = (n as u32 + 64_000, alive.clone());
        insert_process_session(
            name.clone(),
            SessionHandle::TestProcess {
                pid,
                alive: process,
            },
        );
        tmux.take_calls();

        let clear = clear_channel_session_state(
            &http,
            &shared,
            &provider,
            channel,
            "/clear",
            SoftClearNotifyMode::Suppress,
        );
        let cleared = clear.await;
        if case.admitted() {
            assert!(cleared.is_ok(), "{case:?}: {cleared:?}");
            assert!(!alive.load(Ordering::SeqCst), "{case:?}: main kills");
            continue;
        }
        let error = cleared
            .expect_err("a kept session refuses the clear")
            .to_string();
        assert!(error.contains(&name), "{case:?}: {error}");
        let reset = reset_channel_provider_state(
            &http, &shared, &provider, channel, "/restart", true, false, true,
        );
        assert_eq!(reset.await, None, "{case:?}: the reset is refused");
        let pending = &shared.overrides.model_session_reset_pending;
        pending.insert(channel);
        reset_provider_session_if_pending(&http, &shared, &provider, channel, channel).await;
        assert!(
            pending.contains(&channel),
            "{case:?}: the pending reset is kept"
        );
        assert!(
            alive.load(Ordering::SeqCst),
            "{case:?}: the process is kept"
        );
        assert!(session_id(&shared, channel).await, "{case:?}: session kept");
        assert_eq!(
            tmux.take_calls(),
            Vec::<String>::new(),
            "{case:?}: no tmux call"
        );
        crate::services::session_backend::remove_process_session(&name);
    }
    db.drop().await;
}

// The status and health reports show a kept session's host as unsupported without probing
// tmux by its name; a legacy row or no row reads tmux as in main.
#[tokio::test]
async fn reports_name_a_kept_host_without_probing_tmux_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    for (n, case) in Case::ALL.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_302_387_062_000 + n as u64);
        let channel_name = format!("p4c2-report-{n}");
        let name = provider.build_tmux_session_name(&channel_name);
        map_channel(&shared, channel, &channel_name).await;
        case.seed(&pool, &channel_key(&shared, &name), &name, channel.get())
            .await;
        tmux.take_calls();
        let status = build_status_report(&shared, &provider, channel).await;
        let health = build_health_report(&shared, &provider, channel).await;
        let expected = if case.admitted() {
            "`missing`"
        } else {
            "`unsupported-host`"
        };
        for report in [&status, &health] {
            assert!(report.contains(expected), "{case:?}: {report}");
        }
        let probes = tmux.take_calls();
        let named = probes.iter().filter(|call| call.contains(&name)).count();
        assert_eq!(named == 0, !case.admitted(), "{case:?}: {probes:?}");
    }
    db.drop().await;
}
