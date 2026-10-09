use std::sync::Arc;

use poise::serenity_prelude as serenity;
use tracing::Instrument;

use super::session_canonical_identity::HookCanonicalIdentity;
use super::{RoleBinding, SharedData};
use crate::services::provider::ProviderKind;

#[derive(Default)]
pub(super) struct StatusHookTarget<'a> {
    pub(super) canonical: Option<HookCanonicalIdentity<'a>>,
    pub(super) turn_nonce: Option<&'a str>,
}

pub(super) struct TurnTerminalStatus<'a> {
    pub(super) session_key: Option<&'a str>,
    pub(super) name: Option<&'a str>,
    pub(super) model: Option<&'a str>,
    pub(super) status: &'a str,
    pub(super) provider: &'a ProviderKind,
    pub(super) session_info: Option<&'a str>,
    pub(super) tokens: Option<u64>,
    pub(super) cwd: Option<&'a str>,
    pub(super) dispatch_id: Option<&'a str>,
    pub(super) thread_channel_id: Option<u64>,
    pub(super) channel_id: Option<serenity::ChannelId>,
    pub(super) agent_id: Option<&'a str>,
    pub(super) turn_nonce: Option<&'a str>,
}

pub(super) async fn post_status(
    session_key: &str,
    name: Option<&str>,
    model: Option<&str>,
    status: &str,
    provider: &ProviderKind,
    session_info: Option<&str>,
    tokens: Option<u64>,
    cwd: Option<&str>,
    dispatch_id: Option<&str>,
    thread_channel_id: Option<u64>,
    channel_id: Option<serenity::ChannelId>,
    agent_id: Option<&str>,
    target: StatusHookTarget<'_>,
) {
    let StatusHookTarget {
        canonical,
        turn_nonce,
    } = target;
    let status = crate::db::session_status::normalize_incoming_session_status(Some(status));
    let channel_id_string = channel_id.map(|id| id.get().to_string());
    let body = crate::services::dispatched_sessions::HookSessionBody {
        session_key: session_key.to_string(),
        instance_id: None,
        agent_id: agent_id.map(str::to_string),
        status: Some(status.to_string()),
        provider: Some(provider.as_str().to_string()),
        session_info: session_info.map(str::to_string),
        name: clean_nonempty(name).map(str::to_string),
        model: clean_nonempty(model)
            .filter(|value| !value.eq_ignore_ascii_case(provider.as_str()))
            .map(str::to_string),
        tokens,
        cwd: clean_nonempty(cwd).map(str::to_string),
        dispatch_id: clean_nonempty(dispatch_id).map(str::to_string),
        thread_channel_id: thread_channel_id.map(|id| id.to_string()),
        claude_session_id: None,
        session_id: None,
        channel_id: channel_id_string.clone(),
        identity_kind: canonical.map(|identity| identity.identity_kind.to_string()),
        discord_token_hash: canonical.map(|identity| identity.discord_token_hash.to_string()),
        turn_start_nonce: None,
        dispatched_origin: None,
    };

    // The whole block (including the failure `warn!`) is instrumented, not just
    // the request future, so the failure log also carries the span's fields.
    let span = crate::logging::session_span(
        "session_status_hook_post",
        dispatch_id,
        agent_id,
        channel_id_string.map(std::borrow::Cow::Owned),
        Some(session_key),
    );
    async {
        let result = match turn_nonce {
            Some(nonce) => super::internal_api::hook_session_terminal(body, nonce).await,
            None => super::internal_api::hook_session(body).await,
        };
        if let Err(err) = result {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ ADK session POST failed: {err}");
        }
    }
    .instrument(span)
    .await;
}

fn clean_nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

pub(super) async fn post_canonical(
    session_key: Option<&str>,
    name: Option<&str>,
    model: Option<&str>,
    status: &str,
    provider: &ProviderKind,
    session_info: Option<&str>,
    tokens: Option<u64>,
    cwd: Option<&str>,
    dispatch_id: Option<&str>,
    thread_channel_id: Option<u64>,
    channel_id: Option<serenity::ChannelId>,
    agent_id: Option<&str>,
    token_hash: &str,
    scheduled_snapshot: bool,
    _api_port: u16,
) {
    let Some(session_key) = session_key else {
        return;
    };
    let canonical = super::session_canonical_identity::identity_for_session_key(
        session_key,
        provider,
        token_hash,
        scheduled_snapshot,
    );
    post_status(
        session_key,
        name,
        model,
        status,
        provider,
        session_info,
        tokens,
        cwd,
        dispatch_id,
        thread_channel_id,
        channel_id,
        agent_id,
        StatusHookTarget {
            canonical,
            ..Default::default()
        },
    )
    .await;
}

pub(super) async fn post_channel_turn(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    session_key: Option<&str>,
    name: Option<&str>,
    model: Option<&str>,
    provider: &ProviderKind,
    session_info: &str,
    cwd: &str,
    dispatch_id: Option<&str>,
    thread_channel_id: Option<u64>,
    role_binding: Option<&RoleBinding>,
) {
    post_canonical(
        session_key,
        name,
        model,
        "working",
        provider,
        Some(session_info),
        None,
        Some(cwd),
        dispatch_id,
        thread_channel_id,
        Some(channel_id),
        role_binding.map(|binding| binding.role_id.as_str()),
        &shared.token_hash,
        false,
        shared.api_port,
    )
    .await;
}

pub(super) async fn post_legacy(
    session_key: Option<&str>,
    name: Option<&str>,
    model: Option<&str>,
    status: &str,
    provider: &ProviderKind,
    session_info: Option<&str>,
    tokens: Option<u64>,
    cwd: Option<&str>,
    dispatch_id: Option<&str>,
    thread_channel_id: Option<u64>,
    channel_id: Option<serenity::ChannelId>,
    agent_id: Option<&str>,
    _api_port: u16,
) {
    let Some(session_key) = session_key else {
        return;
    };
    post_status(
        session_key,
        name,
        model,
        status,
        provider,
        session_info,
        tokens,
        cwd,
        dispatch_id,
        thread_channel_id,
        channel_id,
        agent_id,
        StatusHookTarget::default(),
    )
    .await;
}

pub(super) async fn post_turn_terminal(request: TurnTerminalStatus<'_>) {
    let Some(session_key) = request.session_key else {
        return;
    };
    post_status(
        session_key,
        request.name,
        request.model,
        request.status,
        request.provider,
        request.session_info,
        request.tokens,
        request.cwd,
        request.dispatch_id,
        request.thread_channel_id,
        request.channel_id,
        request.agent_id,
        StatusHookTarget {
            turn_nonce: request.turn_nonce,
            ..Default::default()
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use std::sync::Mutex;

    #[derive(Clone)]
    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn post_status_span_carries_channel_and_session_identity() {
        const CHILD: &str = "ADK_SESSION_STATUS_HOOK_SPAN_CHILD";
        const TEST: &str = "services::discord::session_status_hook::tests::post_status_span_carries_channel_and_session_identity";
        if std::env::var_os(CHILD).is_none() {
            // internal_api's context is process-global; isolate in a fresh process so
            // this never races a sibling test's context or reaches a live API.
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
            return;
        }

        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(logs.clone());
        // Same directive dcserver ships with, so this proves the span survives
        // the production filter, not just a debug-level test filter.
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_env_filter(tracing_subscriber::EnvFilter::new(
                crate::logging::DEFAULT_TRACING_DIRECTIVE,
            ))
            .with_writer(move || writer.clone())
            .finish();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        crate::logging::test_capture::pin_callsite_interest();
        tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(post_status(
                "session-boundary-4221",
                None,
                None,
                "working",
                &ProviderKind::Claude,
                None,
                None,
                None,
                Some("dispatch-boundary"),
                None,
                Some(serenity::ChannelId::new(4_221_000)),
                Some("agent-boundary"),
                StatusHookTarget::default(),
            ));
        });

        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        // No context installed in this fresh child process, so `load_context()`
        // errs and this failure line is reached deterministically, no socket.
        let line = logs
            .lines()
            .find(|line| line.contains("ADK session POST failed"))
            .expect("failure log line at the production filter level");
        assert!(line.contains("channel_id=Some(\"4221000\")"), "logs={line}");
        assert!(
            line.contains("session_key=Some(\"session-boundary-4221\")"),
            "logs={line}"
        );
        assert!(
            line.contains("dispatch_id=Some(\"dispatch-boundary\")"),
            "logs={line}"
        );
        assert!(
            line.contains("agent_id=Some(\"agent-boundary\")"),
            "logs={line}"
        );
    }
}
