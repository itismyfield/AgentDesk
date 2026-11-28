use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::domain::outbound::port::{DispatchMessagePostError, DispatchMessagePostErrorKind};
use crate::domain::outbound::{
    DeliveryResult, DiscordOutboundPolicy, OutboundDelivery, OutboundIntent, OutboundTargetKey,
    first_raw_message_id,
};
use crate::services::discord::{install_engine_probes, outbound::install_engine_ports};
use crate::services::dispatches::{
    http_outbound::HttpOutbound, thread_archive_defer::should_defer_thread_archive_pg,
};
use crate::services::process::{configure_child_process_group, wait_with_output_timeout};
use crate::test_support::wait::WAIT_CAP;

fn in_fresh_child(name: &str) -> bool {
    const CHILD: &str = "AGENTDESK_ENGINE_PROBE_TEST_CHILD";
    let test = format!("engine_probe_tests::{name}");
    if std::env::var(CHILD).as_deref() == Ok(test.as_str()) {
        return true;
    }

    let root = tempfile::tempdir().unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", &test, "--nocapture"])
        .env(CHILD, &test)
        .env("AGENTDESK_ROOT_DIR", root.path())
        .env_remove("AGENTDESK_CONFIG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_child_process_group(&mut command);
    let output = wait_with_output_timeout(command.spawn().unwrap(), WAIT_CAP, &test).unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success()
            && stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored"),
        "{test}: {}\n{stdout}\n{stderr}",
        output.status
    );
    false
}

#[test]
fn inflight_remove_requires_installation() {
    if !in_fresh_child("inflight_remove_requires_installation") {
        return;
    }
    use serde_json::{Value, json};

    let root = crate::config::runtime_root().unwrap();
    let row = root.join("runtime/discord_inflight/claude/901.json");
    let bytes = br#"{"channel_id":901,"user_msg_id":7}"#;
    std::fs::create_dir_all(row.parent().unwrap()).unwrap();
    std::fs::write(&row, bytes).unwrap();

    let runtime = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&runtime).unwrap();
    context.with(|ctx| {
        ctx.globals()
            .set("agentdesk", rquickjs::Object::new(ctx.clone()).unwrap())
            .unwrap();
        crate::engine::ops::exec_ops::register_exec_ops(&ctx).unwrap();
        let remove = || -> Value {
            let output: String = ctx
                .eval("JSON.stringify(agentdesk.inflight.remove('claude', '901'))")
                .unwrap();
            serde_json::from_str(&output).unwrap()
        };
        assert_eq!(
            remove(),
            json!({"ok": false, "reason": "Io", "error": "inflight store not installed"})
        );
        assert_eq!(std::fs::read(&row).unwrap().as_slice(), bytes);

        install_engine_probes();
        assert_eq!(
            remove(),
            json!({"ok": true, "removed": row.display().to_string()})
        );
        assert!(!row.exists());
        assert_eq!(remove(), json!({"ok": false, "error": "not found"}));
    });
}

#[tokio::test]
async fn archive_defer_requires_installation_to_archive_missing_rows() {
    if !in_fresh_child("archive_defer_requires_installation_to_archive_missing_rows") {
        return;
    }
    assert_eq!(should_defer_thread_archive_pg(None, "901").await, Ok(true));
    install_engine_probes();
    assert_eq!(should_defer_thread_archive_pg(None, "901").await, Ok(false));
}

#[tokio::test]
async fn outbound_requires_installation() {
    if !in_fresh_child("outbound_requires_installation") {
        return;
    }
    use axum::{
        Json, Router,
        extract::State,
        routing::{patch, post},
    };
    use serde_json::{Value, json};

    async fn receive(State(hits): State<Arc<AtomicUsize>>, Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(body["content"], "engine probe");
        hits.fetch_add(1, Ordering::Relaxed);
        Json(json!({"id": "9001"}))
    }

    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/channels/123/messages", post(receive))
        .route("/channels/123/messages/9001", patch(receive))
        .with_state(hits.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let outbound = HttpOutbound::new(
        reqwest::Client::builder().no_proxy().build().unwrap(),
        "test-token",
        base,
    );
    let intent = OutboundIntent::new(
        "engine-probe",
        "send",
        "engine probe",
        OutboundTargetKey::channel(123),
        DiscordOutboundPolicy::review_notification(),
    );
    let edit = || outbound.clone().edit_message("123", "9001", "engine probe");
    let missing = "discord http outbound not installed";
    assert_eq!(
        edit().await,
        Err(DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            missing.to_string()
        ))
    );
    assert_eq!(
        outbound.deliver(intent.clone()).await,
        DeliveryResult::PermanentFailure {
            reason: missing.to_string()
        }
    );
    assert_eq!(hits.load(Ordering::Relaxed), 0);

    install_engine_ports();
    let delivery = outbound.deliver(intent).await;
    let DeliveryResult::Sent { messages, .. } = delivery else {
        panic!("expected a delivered message: {delivery:?}");
    };
    assert_eq!(first_raw_message_id(&messages).as_deref(), Some("9001"));
    assert_eq!(edit().await.unwrap(), "9001");
    assert_eq!(hits.load(Ordering::Relaxed), 2);
    server.abort();
}
