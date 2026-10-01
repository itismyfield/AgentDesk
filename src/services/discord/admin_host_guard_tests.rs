//! Admin commands and diagnostics on every stored host case, through their real entries.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{Method, StatusCode, Uri};
use axum::response::IntoResponse;
use poise::serenity_prelude::{ChannelId, Http, HttpBuilder};

use crate::services::discord::admin_host_guard::ManagedReset;
use crate::services::discord::commands::{
    SoftClearNotifyMode, build_health_report, build_status_report, clear_channel_session_state,
    reset_channel_provider_state, reset_provider_session_if_pending,
};
use crate::services::discord::host_defer_gate::tests::{
    Case, Nameless, ScriptedTmux, map_channel, postgres, with_second_bot,
};
use crate::services::discord::host_teardown_gate::test_support::{channel_key, shared_on};
use crate::services::provider::ProviderKind;
use crate::services::session_backend::{SessionHandle, insert_process_session};

/// A local stand-in for Discord and the runtime API logging each request as `METHOD path body`.
pub(crate) struct Recorder {
    pub(crate) http: Arc<Http>,
    pub(crate) port: u16,
    calls: Arc<Mutex<Vec<String>>>,
    server: tokio::task::AbortHandle,
}

impl Recorder {
    pub(crate) async fn start() -> Self {
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorded = calls.clone();
        let app = axum::Router::new().fallback(axum::routing::any(
            move |method: Method, uri: Uri, body: Bytes| {
                let recorded = recorded.clone();
                async move {
                    let body = String::from_utf8_lossy(&body);
                    let call = format!("{method} {} {body}", uri.path());
                    recorded.lock().unwrap().push(call);
                    if method == Method::DELETE {
                        return StatusCode::NO_CONTENT.into_response();
                    }
                    axum::Json(serde_json::json!({
                        "id": "900001", "channel_id": "1", "content": "",
                        "author": {"id": "1", "username": "t", "discriminator": "0001", "avatar": null},
                        "timestamp": "2026-10-02T00:00:00+00:00", "edited_timestamp": null,
                        "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
                        "attachments": [], "embeds": [], "pinned": false, "type": 0
                    }))
                    .into_response()
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let http = HttpBuilder::new("test-token")
            .proxy(format!("http://127.0.0.1:{port}"))
            .ratelimiter_disabled(true)
            .build();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let server = server.abort_handle();
        let http = Arc::new(http);
        Self {
            http,
            port,
            calls,
            server,
        }
    }

    pub(crate) fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Whether this process runs the test body: the parent re-runs `test` alone in a child, as
/// the runtime API the body points at its recorder is process-global.
pub(crate) fn api_child(test: &str) -> bool {
    const CHILD: &str = "ADK_ADMIN_HOST_GUARD_API_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let exe = std::env::current_exe().expect("test binary");
    let output = std::process::Command::new(exe)
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .expect("child test run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let passed = output.status.success() && stdout.contains("1 passed");
    // The child's panic leads, so a missing test database reads as such before its summary.
    assert!(passed, "{stderr}\n{stdout}");
    false
}

async fn session_id(shared: &crate::services::discord::SharedData, channel: ChannelId) -> bool {
    let core = shared.core.lock().await;
    core.sessions[&channel].session_id.is_some()
}

/// A live process session under `name` whose kill the returned flag observes.
pub(crate) fn process(name: &str, pid: u32) -> Arc<AtomicBool> {
    let alive = Arc::new(AtomicBool::new(true));
    let handle = SessionHandle::TestProcess {
        pid,
        alive: alive.clone(),
    };
    insert_process_session(name.to_string(), handle);
    alive
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
        let alive = process(&name, n as u32 + 64_000);
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
        let reset = reset.await;
        let refused = matches!(&reset, ManagedReset::Refused(reason) if reason.contains(&name));
        assert!(refused, "{case:?}: {reset:?}");
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

// A channel holding no name is judged by its own row before `/clear` changes anything: only
// a found legacy row, or no row with nothing in flight, clears as in main.
#[tokio::test]
async fn clear_judges_a_nameless_channel_by_its_own_row_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let (shared, _registry) = with_second_bot(&pool, "p4c2-second-bot").await;
    let (provider, http) = (ProviderKind::Claude, Arc::new(Http::new("")));
    let own = shared.token_hash.clone();
    for (n, case) in Nameless::ALL.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_302_387_066_000 + n as u64);
        let name = provider.build_tmux_session_name(&format!("p4c2-nameless-{n}"));
        map_channel(&shared, channel, "unnamed").await;
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        (session.channel_name, session.session_id) = (None, Some("sid".into()));
        drop(core);
        case.seed(&pool, &own, "p4c2-second-bot", channel.get(), &name)
            .await;
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
        if matches!(case, Nameless::Legacy | Nameless::Missing) {
            assert!(cleared.is_ok(), "{case:?}: {cleared:?}");
            assert!(!session_id(&shared, channel).await, "{case:?}: main clears");
            continue;
        }
        let error = cleared.expect_err("a nameless kept channel refuses the clear");
        assert!(error.to_string().contains("채널 이름"), "{case:?}: {error}");
        assert!(session_id(&shared, channel).await, "{case:?}: session kept");
        let core = shared.core.lock().await;
        assert!(!core.sessions[&channel].cleared, "{case:?}: not cleared");
        drop(core);
        let calls = tmux.take_calls();
        assert_eq!(calls, Vec::<String>::new(), "{case:?}: no tmux call");
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
