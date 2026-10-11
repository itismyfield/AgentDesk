//! A launched Herdr source reaches O by its first activation: launch, SessionStart hook and attach,
//! then a storeless writer host whose actor applies the source's bind before it captures and posts.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use super::{
    CHANNEL_ID, LaunchedPane, OHost, Successor, Terminating, append, assistant, posted_in_ledger,
    root, segments_of,
};
use crate::db::dispatched_sessions::hosted_execution::{HostedLocation, HostedOwner};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude_tui::hook_server::observation_ingress::{
    IngressOutcome, ProceedReason, observe_binding_hook,
};
use crate::services::discord::tui_prompt_relay::herdr_source::{
    HerdrSourceAttach, attach_launched_herdr_source,
};
use crate::services::herdr_launch::{
    HerdrLaunch, HerdrLaunchCommand, HerdrLaunchEndpoint, HerdrLaunchOutcome, launch_herdr_session,
};
use crate::services::provider::ProviderKind;
use crate::services::session_host::herdr_terminate_real_tests::legacy_row;
use crate::services::session_host::herdr_termination_rig as herdr;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::store::OStore;
use crate::services::tui_o::writer::WriterAlarm;
use crate::services::tui_prompt_dedupe::binding_context::{
    BINDING_HEADER, HookBindingEnvelope, PreparedIncarnation,
};
use crate::services::tui_prompt_dedupe::binding_events::{self, BindingTarget};

const FIRST: &str = "the launched execution's first answer";

#[test]
fn act_o_first_activation_applies_the_launched_source_pg() {
    let path = concat!(
        module_path!(),
        "::act_o_first_activation_applies_the_launched_source_pg"
    );
    if !herdr::in_child(path, CHANNEL_ID) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(CHANNEL_ID);
    let events = tempfile::tempdir().unwrap();
    let events_root = events.path().to_path_buf();
    let rig_install = rig.thread_installer();
    // No store yet: every thread reads the channel as a candidate its host adopts first.
    let install = Arc::new(move || {
        rig_install();
        binding_events::set_test_root(Some(&events_root));
        let candidate = [(CHANNEL_ID, RuntimeHandoffKind::ClaudeTui)];
        std::mem::forget(
            crate::services::tui_o::cutover::test_override::force_candidates(&candidate),
        );
    });
    install();
    let on_start = Arc::clone(&install);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(move || on_start())
        .build()
        .unwrap()
        .block_on(first_activation(rig));
}

async fn first_activation(rig: herdr::P11TerminationRig) {
    let (harness, database, pool, session_key) = Terminating::open(&ProviderKind::Claude).await;
    let token_hash = harness.shared.token_hash.clone();
    legacy_row(&pool, &session_key, &token_hash, CHANNEL_ID).await;
    let record = rig.record(&token_hash, &session_key);
    let (owner, at) = (record.owner, record.location.expect("the rig's endpoint"));
    let a_session = uuid::Uuid::new_v4().to_string();
    let a_path = root().join("a.jsonl");
    std::fs::write(&a_path, b"").unwrap();
    let (nonce, location, context) = launch(&pool, &owner, &at, &a_session).await;

    // The launch session's SessionStart: the ingress takes it as the current execution's own.
    let started = session_start(&context, &a_session, &a_path);
    assert_eq!(
        started,
        IngressOutcome::Proceed(ProceedReason::NoSessionSwitch)
    );
    let reader = LaunchedPane::successor(&location, &nonce, "claude", &a_session);
    let nonce = nonce.as_str();
    let attached = attach_launched_herdr_source(
        &pool, &owner, CHANNEL_ID, nonce, &a_session, &a_path, &reader,
    )
    .await;
    assert!(
        matches!(attached, HerdrSourceAttach::Published { bound: true, .. }),
        "{attached:?}"
    );
    let row = herdr::row(&pool, &session_key).await;
    assert_eq!(row, Some((herdr::HostedState::Bound, nonce.to_string())));
    let a = source_id_for(&a_session, &a_path).unwrap();
    let logged = binding_events::binding_events_since(CHANNEL_ID, 0).unwrap();
    let [bind] = logged.as_slice() else {
        panic!("one logged bind: {logged:?}");
    };
    assert_eq!(bind.new, BindingTarget::Source(a.clone()), "{bind:?}");
    assert_eq!(bind.execution_nonce.as_deref(), Some(nonce));
    assert!(
        OStore::existing(&root()).is_none(),
        "no store before the host"
    );

    // The host adopts the channel while A is still empty, as before Claude's first output; its
    // actor then applies A's bind, captures A and posts.
    let mut o = OHost::new();
    o.start();
    o.view().await;
    append(&a_path, &assistant("m1", FIRST));
    let reached = o.until(|posts| !posts.is_empty()).await;
    o.stop().await;
    let bytes = std::fs::read(&a_path).unwrap();
    let (checkpoint, cursor) = applied(&a);
    let captured = Some((bytes.len() as u64, hex::encode(Sha256::digest(&bytes))));
    assert_eq!(checkpoint, Some(bind.seq), "the actor applied A's bind");
    assert_eq!(cursor, captured, "A's cursor took all of A");
    assert!(!segments_of(&a).is_empty(), "A's records were spooled");
    assert!(reached, "{:?} {:?}", o.posts(), o.halts());
    assert_eq!(o.posts(), [FIRST]);
    assert_eq!(posted_in_ledger(FIRST), 1);
    assert_eq!(o.halts(), Vec::<WriterAlarm>::new());
    drop(harness);
    pool.close().await;
    database.drop().await;
}

/// A's production launch at the rig's endpoint `at`: its nonce, its pane, and the launch context
/// the pane's hooks carry.
async fn launch(
    pool: &sqlx::PgPool,
    owner: &HostedOwner,
    at: &HostedLocation,
    session: &str,
) -> (String, HostedLocation, PathBuf) {
    let cwd = root().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let context = Mutex::new(None);
    let prepare = |prepared: &PreparedIncarnation| {
        *context.lock().unwrap() = Some(prepared.path.clone());
        let config = crate::services::claude_tui::session::ClaudeTuiLaunchConfig {
            tmux_session_name: prepared.context.tmux_session.clone(),
            working_dir: cwd.clone(),
            claude_bin: crate::services::claude_command::ClaudeBinary::from_tmux_wrapper_argv(
                "/usr/local/bin/claude",
            ),
            agentdesk_exe: PathBuf::from("/usr/local/bin/agentdesk"),
            hook_endpoint: "http://127.0.0.1:1".into(),
            session_id: session.to_owned(),
            system_prompt: None,
            model: None,
            resume: false,
        };
        let files = crate::services::claude_tui::session::prepare_claude_tui_launch(&config)?;
        Ok(HerdrLaunchCommand {
            cwd: cwd.clone(),
            command: format!("bash {}", files.launch_script_path.display()),
        })
    };
    let launch = HerdrLaunch {
        endpoint: Some(HerdrLaunchEndpoint {
            execution_node: at.execution_node.clone(),
            config_key: at.endpoint_config_key.clone(),
            socket_addr: at.socket_addr.clone(),
            herdr_session: at.named_session.clone(),
        }),
        owner: owner.clone(),
        channel_id: Some(CHANNEL_ID),
        expected_native_session_id: Some(session.to_owned()),
        resume: false,
        provider_root: None,
    };
    let host = Arc::new(Successor("claude"));
    let outcome = launch_herdr_session(pool, launch, prepare, host).await;
    let Ok(HerdrLaunchOutcome::Launched {
        execution_nonce,
        location,
        evidence: Ok(()),
    }) = outcome
    else {
        panic!("{outcome:?}");
    };
    let context = context.lock().unwrap().clone().expect("A's launch context");
    (execution_nonce, location, context)
}

/// Claude's SessionStart of the launch session as its hook relays it under launch context `context`.
fn session_start(context: &Path, session: &str, path: &Path) -> IngressOutcome {
    let envelope = HookBindingEnvelope::capture_from_env("claude", |name| {
        (name == "AGENTDESK_BINDING_CONTEXT").then(|| context.as_os_str().to_owned())
    });
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let payload = serde_json::json!({"session_id": session, "source": "startup",
        "hook_event_name": "SessionStart", "transcript_path": path});
    observe_binding_hook(
        "claude",
        "SessionStart",
        Some(session),
        Some(session),
        &payload,
        &headers,
    )
}

/// O's applied binding seq for the channel, and `source`'s cursor as `(captured, prefix hash)`.
fn applied(source: &SourceId) -> (Option<u64>, Option<(u64, String)>) {
    let store = OStore::existing(&root()).expect("the host created the store");
    let era = store
        .read_era()
        .unwrap()
        .expect("the first activation began an era");
    let channel = store.open_channel(&era, CHANNEL_ID).unwrap();
    let channel = channel.expect("the channel's store");
    let cursor = channel.cursor(source);
    let cursor = cursor.map(|cursor| (cursor.captured_through, cursor.prefix_hash.clone()));
    (channel.binding_checkpoint().unwrap(), cursor)
}
