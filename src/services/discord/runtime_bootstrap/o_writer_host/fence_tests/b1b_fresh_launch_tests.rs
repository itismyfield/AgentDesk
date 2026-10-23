//! A selected real O gateway host admits and delivers the real absent Codex FIFO.

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::CodexTui;
use crate::services::codex::CodexSourceMode;
use crate::services::codex_tui::canary::{CANARY_CHANNEL, CANARY_TMUX};
use crate::services::discord::commands::control::codex_clear_relaunch_tests::{
    CanaryFixture, NEW_NATIVE_ID, absent_launch_tests::auxiliary_tests as b1,
};
use crate::services::discord::router::TurnKind;
use crate::services::discord::router::message_handler::{IntakeDeps, IntakeRequest};
use crate::services::discord::router::{IntakeOrigin, IntakeSubmission, admit_text_intake};
use crate::services::tui_o::cutover::{self, test_override};
use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource, capture::SourceCapture};
use crate::services::tui_o::store::rotation::{Boundary, SourceLink};
use crate::services::tui_o::store::spool::source_key;
use crate::services::tui_o::store::{InitSource, Initialized, OStore, StoreConfig};
use crate::services::tui_prompt_dedupe as dedupe;
use futures::FutureExt;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, atomic::Ordering, mpsc};

const FIRST_REPLY: &str = "B1b fresh O first reply";
const SECOND_REPLY: &str = "clear relaunch completed";
const OLD_REPLY: &str = "B1b suspended historical reply";

#[path = "b1b_joint_owner_tests.rs"]
mod joint_foreign;
#[path = "b1b_terminal_tests.rs"]
mod native_output;
mod ordinary_restart_tests;
#[path = "b1b_repeated_absence_tests.rs"]
mod repeated_absence;
#[path = "b1b_restart_phase_tests.rs"]
mod restart_phase;

#[derive(Clone, Copy)]
enum BeforeInput {
    Absent,
    ForeignInventory,
    Restart,
    ExplicitClear,
    RepeatAbsent,
    InPlaceAbsent,
    InPlaceClear,
    UnprovedThenClear,
    RestartAfterContext,
    RestartAfterProof,
    RestartAfterFirstPost,
}

struct Discord {
    http: Arc<serenity::Http>,
    messages: Arc<Mutex<Vec<serde_json::Value>>>,
    server: tokio::task::AbortHandle,
    history_reads: Arc<std::sync::atomic::AtomicUsize>,
}

impl Discord {
    async fn start() -> Self {
        use axum::{
            body::Bytes,
            http::{Method, Uri},
            response::IntoResponse,
        };
        let messages: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
        let recorded = Arc::clone(&messages);
        let history_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reads = Arc::clone(&history_reads);
        let app = axum::Router::new().fallback(axum::routing::any(
            move |method: Method, uri: Uri, body: Bytes| {
                let recorded = Arc::clone(&recorded);
                let reads = Arc::clone(&reads);
                async move {
                    if uri.path().ends_with("/users/@me") {
                        return axum::Json(json!({"id":"1", "username":"o-bot",
                            "discriminator":"0001", "avatar":null, "bot":true})).into_response();
                    }
                    if method == Method::GET {
                        if uri.path() == format!("/api/v10/channels/{CANARY_CHANNEL}/messages") {
                            reads.fetch_add(1, Ordering::SeqCst);
                        }
                        return axum::Json(json!(*recorded.lock().unwrap())).into_response();
                    }
                    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    let mut messages = recorded.lock().unwrap();
                    let message = json!({
                        "id":(900_001 + messages.len() as u64).to_string(),
                        "channel_id":CANARY_CHANNEL.to_string(), "content":payload["content"],
                        "author":{"id":"1","username":"o-bot","discriminator":"0001","avatar":null,"bot":true},
                        "timestamp":"2026-10-20T00:00:00+00:00", "edited_timestamp":null,
                        "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
                        "attachments":[],"embeds":[],"pinned":false,"type":0
                    });
                    messages.push(message.clone());
                    axum::Json(message).into_response()
                }
            },
        ));
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy(format!("http://127.0.0.1:{port}"))
                .ratelimiter_disabled(true)
                .build(),
        );
        let server = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
        Self {
            http,
            messages,
            server: server.abort_handle(),
            history_reads,
        }
    }

    fn texts(&self) -> Vec<String> {
        self.messages
            .lock()
            .unwrap()
            .iter()
            .map(|message| message["content"].as_str().unwrap().to_owned())
            .collect()
    }
}

impl Drop for Discord {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn input() -> IntakeSubmission {
    let message = serenity::MessageId::new(CANARY_CHANNEL + 101);
    IntakeSubmission::text_for_tests(
        ProviderKind::Codex,
        IntakeRequest {
            intake_outbox_id: None,
            channel_id: serenity::ChannelId::new(CANARY_CHANNEL),
            user_msg_id: message,
            source_message_ids: vec![message],
            busy_followup_retry_user_msg_id: message,
            request_owner: serenity::UserId::new(1),
            request_owner_name: "restored-owner".into(),
            user_text: b1::Q1.into(),
            reply_to_user_message: false,
            defer_watcher_resume: false,
            wait_for_completion: false,
            merge_consecutive: false,
            reply_context: None,
            has_reply_boundary: false,
            dm_hint: Some(false),
            turn_kind: TurnKind::Foreground,
            preserve_on_cancel: false,
        },
        IntakeOrigin::QueuedDrain(None),
    )
}

fn subtree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    if !root.exists() {
        return files;
    }
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(subtree(&path));
        } else {
            files.insert(path.clone(), fs::read(path).unwrap());
        }
    }
    files
}

fn source_spool(root: &Path, key: &str) -> BTreeMap<PathBuf, Vec<u8>> {
    subtree(&root.join("spool"))
        .into_iter()
        .filter(|(path, _)| {
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(&format!("{key}-"))
        })
        .collect()
}

async fn eventually(mut done: impl FnMut() -> bool, what: &str) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

async fn receive<T>(receiver: &mpsc::Receiver<T>, what: &str) -> T {
    let mut value = None;
    eventually(
        || {
            value = receiver.try_recv().ok();
            value.is_some()
        },
        what,
    )
    .await;
    value.unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn actual_gateway_empty_store_launches_fresh_then_warm_fifo_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::actual_gateway_empty_store_launches_fresh_then_warm_fifo_pg"
    )) {
        scenario(false, BeforeInput::Absent).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_gateway_retained_cursor_launches_fresh_without_old_consumption_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::actual_gateway_retained_cursor_launches_fresh_without_old_consumption_pg"
    )) {
        scenario(true, BeforeInput::Absent).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_gateway_restart_before_proof_preserves_fresh_fifo_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::actual_gateway_restart_before_proof_preserves_fresh_fifo_pg"
    )) {
        scenario(true, BeforeInput::Restart).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_gateway_explicit_clear_discards_old_queue_then_new_fifo_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::actual_gateway_explicit_clear_discards_old_queue_then_new_fifo_pg"
    )) {
        scenario(true, BeforeInput::ExplicitClear).await;
    }
}

async fn scenario(retained: bool, before: BeforeInput) {
    let mut fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let api = b1::recorder(&mut fixture).await;
    let _foreign_owners = if matches!(before, BeforeInput::ForeignInventory) {
        Some(joint_foreign::install(&mut fixture).await)
    } else {
        None
    };
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://127.0.0.1:{}",
        api.port
    ));
    let _cli = fixture.cold_cli();
    let _routing = fixture.scoped_test_env("ADK_INTAKE_ROUTING_MODE", "disabled".as_ref());
    let _writer = fixture.scoped_test_env(test_override::CHILD_ENV, "1".as_ref());
    let channels = serde_json::to_string(&[(CANARY_CHANNEL, CodexTui)]).unwrap();
    let _channels = fixture.scoped_test_env(test_override::CHANNELS_ENV, channels.as_ref());
    let _selected = test_override::force_channels(&[(CANARY_CHANNEL, CodexTui)]);
    if !matches!(
        before,
        BeforeInput::InPlaceAbsent | BeforeInput::InPlaceClear
    ) {
        b1::target_missing(&fixture, true);
    }
    crate::services::platform::tmux::invalidate_availability_cache();
    assert_eq!(
        dedupe::codex_verified_channel_delivery_allowed(CANARY_CHANNEL),
        matches!(
            before,
            BeforeInput::InPlaceAbsent | BeforeInput::InPlaceClear
        ),
        "the ordinary in-place case must begin with real current N2 permission"
    );
    if retained {
        b1::terminal(&fixture.old_source.path, "historical-turn", OLD_REPLY);
    }
    let old_bytes = fs::read(&fixture.old_source.path).unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, fixture.root.path())
        .unwrap()
        .unwrap();
    let sources = retained
        .then(|| InitSource {
            source_id: fixture.old_source.clone(),
            delivery_start: 0,
            prefix_hash: hex::encode(Sha256::digest([])),
        })
        .into_iter()
        .collect::<Vec<_>>();
    let era = store
        .begin_era(&[CANARY_CHANNEL], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: sources.clone(),
                initial_anchor: 0,
                build_digest: "b1b-actual-gateway".into(),
                at: chrono::Utc::now(),
            })
        })
        .unwrap();
    let mut initial = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
    if retained {
        let mut capture = SourceCapture::open(fixture.old_source.clone(), 0).unwrap();
        let CaptureOutcome::Batch(batch) = capture.poll(1024 * 1024) else {
            panic!("clean historical source must capture");
        };
        initial
            .append_spool(&batch, &capture.prefix_hash())
            .unwrap();
        let proof = dedupe::binding_events::codex::read_ownership(&fixture.context)
            .unwrap()
            .verified
            .unwrap();
        let mut rotation = initial.rotation().unwrap();
        rotation.links.insert(
            source_key(&fixture.old_source),
            SourceLink {
                source: fixture.old_source.clone(),
                seq: proof.seq,
                parent: None,
                committed_at: fixture.context.created_at,
                boundary: Boundary::Owed {
                    from: old_bytes.len() as u64,
                },
                hot_candidate: None,
            },
        );
        initial.write_rotation(&rotation).unwrap();
        initial.set_binding_checkpoint(proof.seq).unwrap();
    }
    let old_cursor = initial.cursor(&fixture.old_source).cloned();
    let old_rotation = initial.rotation().unwrap();
    drop(initial);
    let old_key = source_key(&fixture.old_source);
    let channel_dir = fixture
        .root
        .path()
        .join("o_store")
        .join(CANARY_CHANNEL.to_string());
    let old_spool = source_spool(&channel_dir, &old_key);
    assert_eq!(
        !old_spool.is_empty(),
        retained,
        "O2 must preserve actual retained spool bytes"
    );
    let output = Discord::start().await;
    let mut old_io = (!matches!(
        before,
        BeforeInput::InPlaceAbsent | BeforeInput::InPlaceClear
    ))
    .then(|| {
        crate::services::codex::actual_submit_tests::source_io::Watch::start(
            &fixture.old_source.path,
        )
    });
    let prepared = parts_with_rest(
        &fixture.shared,
        &ProviderKind::Codex,
        None,
        Some(Arc::clone(&output.http)),
    );
    prepared.gate.acquired();
    let readiness = host::process_readiness();
    let mut handles = host::start_managed(
        ShadowProvider::Codex,
        true,
        cutover::boot_ownership(),
        || prepared,
    );
    assert_eq!(handles.len(), 1);
    let run = std::panic::AssertUnwindSafe(async {
        eventually(
            || readiness.submission(CANARY_CHANNEL).is_some(),
            "the first actual O actor",
        )
        .await;
        match before {
            BeforeInput::Absent
            | BeforeInput::ForeignInventory
            | BeforeInput::RepeatAbsent
            | BeforeInput::InPlaceAbsent
            | BeforeInput::RestartAfterContext
            | BeforeInput::RestartAfterProof
            | BeforeInput::RestartAfterFirstPost => {}
            BeforeInput::Restart => {
                let old_generation = handles[0].generation();
                tokio::time::timeout(
                    Duration::from_secs(20),
                    handles.pop().unwrap().stop_and_join(),
                )
                .await
                .expect("the pre-proof host must stop")
                .unwrap();
                assert!(!readiness.is_hosted(CANARY_CHANNEL));
                let restarted = parts_with_rest(
                    &fixture.shared,
                    &ProviderKind::Codex,
                    None,
                    Some(Arc::clone(&output.http)),
                );
                handles = host::start_managed(
                    ShadowProvider::Codex,
                    true,
                    cutover::boot_ownership(),
                    || restarted,
                );
                assert_eq!(handles.len(), 1);
                assert_ne!(handles[0].generation(), old_generation);
            }
            BeforeInput::ExplicitClear
            | BeforeInput::InPlaceClear
            | BeforeInput::UnprovedThenClear => {
                let current_actor = readiness.submission(CANARY_CHANNEL).unwrap();
                let current_generation = handles[0].generation();
                if matches!(before, BeforeInput::UnprovedThenClear) {
                    repeated_absence::unproved_successor_holds_actual_input(
                        &fixture, &api, &output, &readiness,
                    )
                    .await;
                }
                if matches!(before, BeforeInput::InPlaceClear) {
                    eventually(
                        || readiness.is_ready(CANARY_CHANNEL) && readiness.accepts(CANARY_CHANNEL),
                        "ordinary N2 full resume before explicit clear",
                    )
                    .await;
                    assert!(dedupe::codex_verified_o_source_allowed(
                        CANARY_CHANNEL,
                        &fixture.old_source,
                    ));
                    assert!(
                        output.texts().is_empty(),
                        "the inherited prefix owes no POST"
                    );
                    assert!(b1::queue_identities(&fixture).await.is_empty());
                }
                let discarded = b1::restore_instructions(
                    &mut fixture,
                    vec![b1::instruction(
                        CANARY_CHANNEL + 90,
                        "discard this pre-clear input",
                    )],
                )
                .await;
                assert_eq!(b1::queue_identities(&fixture).await, discarded);
                crate::services::discord::commands::clear_channel_session_state(
                    &api.http,
                    &fixture.shared,
                    &ProviderKind::Codex,
                    serenity::ChannelId::new(CANARY_CHANNEL),
                    "!clear",
                    crate::services::discord::commands::SoftClearNotifyMode::Suppress,
                )
                .await
                .unwrap();
                assert!(
                    b1::queue_identities(&fixture).await.is_empty(),
                    "explicit clear must discard its old queue"
                );
                assert!(dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
                assert_eq!(handles[0].generation(), current_generation);
                assert!(current_actor.same_run(&readiness.submission(CANARY_CHANNEL).unwrap()));
            }
        }
        eventually(
            || readiness.submission(CANARY_CHANNEL).is_some(),
            "the selected actual O actor after the pre-input stage",
        )
        .await;
        let generation = handles[0].generation();
        let actor = readiness.submission(CANARY_CHANNEL).unwrap();
        let fresh = run_fifo(
            &mut fixture,
            &api,
            &output,
            &readiness,
            &mut old_io,
            before,
            &mut handles,
        )
        .await;
        let restarted = matches!(
            before,
            BeforeInput::RestartAfterContext
                | BeforeInput::RestartAfterProof
                | BeforeInput::RestartAfterFirstPost
        );
        assert_eq!(handles[0].generation() != generation, restarted);
        assert_eq!(
            !actor.same_run(&readiness.submission(CANARY_CHANNEL).unwrap()),
            restarted
        );
        if matches!(before, BeforeInput::RepeatAbsent) {
            let preserved = repeated_absence::repeat_absence(
                &mut fixture,
                &api,
                &output,
                &readiness,
                &store,
                &era,
                &fresh,
            )
            .await;
            assert_eq!(handles[0].generation(), generation);
            assert!(actor.same_run(&readiness.submission(CANARY_CHANNEL).unwrap()));
            Some(preserved)
        } else {
            None
        }
    })
    .catch_unwind()
    .await;
    if let Some(token) = fixture
        .shared
        .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
        .snapshot()
        .await
        .cancel_token
    {
        token.cancelled.store(true, Ordering::SeqCst);
        b1::wait_idle(&fixture).await;
    }
    let stopped = match handles.pop() {
        Some(handle) => {
            Some(tokio::time::timeout(Duration::from_secs(20), handle.stop_and_join()).await)
        }
        None => None,
    };
    let preserved = run.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    stopped
        .expect("the fixture must retain its active host")
        .expect("the real O host must stop")
        .unwrap();
    let recovered = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
    if let Some(preserved) = preserved {
        preserved.assert_preserved(&recovered);
    }
    assert_eq!(recovered.cursor(&fixture.old_source).cloned(), old_cursor);
    assert_eq!(
        recovered.rotation().unwrap().links.get(&old_key),
        old_rotation.links.get(&old_key)
    );
    assert_eq!(
        recovered.rotation().unwrap().successors.get(&old_key),
        old_rotation.successors.get(&old_key)
    );
    assert_eq!(source_spool(&channel_dir, &old_key), old_spool);
    assert_eq!(fs::read(&fixture.old_source.path).unwrap(), old_bytes);
    assert_eq!(
        old_io.as_ref().unwrap().stats(),
        Default::default(),
        "old source IO stays zero from its historical observation boundary through shutdown"
    );
    assert_eq!(
        output
            .texts()
            .iter()
            .filter(|text| text.contains(FIRST_REPLY))
            .count(),
        1
    );
    assert_eq!(
        output
            .texts()
            .iter()
            .filter(|text| text.contains(SECOND_REPLY))
            .count(),
        1
    );
    assert!(output.texts().iter().all(|text| !text.contains(OLD_REPLY)));
    assert_eq!(
        output
            .texts()
            .iter()
            .filter(|text| text.as_str() == "-# ✅ 완료")
            .count(),
        if matches!(before, BeforeInput::RepeatAbsent) {
            3
        } else {
            2
        },
        "each actual fresh response has one completion summary"
    );
    assert!(recovered.ledger().unresolved().is_none());
    assert_eq!(
        recovered.ledger().next_serial(),
        if matches!(before, BeforeInput::RepeatAbsent) {
            6
        } else {
            4
        }
    );
    for (serial, reply) in [
        (0, FIRST_REPLY),
        (1, "-# ✅ 완료"),
        (2, SECOND_REPLY),
        (3, "-# ✅ 완료"),
    ] {
        let piece = recovered.ledger().piece(serial).unwrap();
        assert!(piece.payload.contains(reply));
        assert!(matches!(
            piece.outcome,
            Some(crate::services::tui_o::store::ledger::PieceOutcome::Posted(
                _
            ))
        ));
    }
}

async fn run_fifo(
    fixture: &mut CanaryFixture,
    api: &crate::services::discord::admin_host_guard::tests::Recorder,
    output: &Discord,
    readiness: &Arc<host::Readiness>,
    old_io: &mut Option<crate::services::codex::actual_submit_tests::source_io::Watch>,
    before: BeforeInput,
    handles: &mut Vec<host::ManagedWriterHandle>,
) -> dedupe::binding_context::BindingContext {
    eventually(
        || readiness.submission(CANARY_CHANNEL).is_some(),
        "actual GatewayHost live actor",
    )
    .await;
    assert!(readiness.is_hosted(CANARY_CHANNEL));
    if matches!(before, BeforeInput::InPlaceAbsent) {
        eventually(
            || readiness.is_ready(CANARY_CHANNEL) && readiness.accepts(CANARY_CHANNEL),
            "ordinary N2 full resume before the real target disappears",
        )
        .await;
        assert!(dedupe::codex_verified_o_source_allowed(
            CANARY_CHANNEL,
            &fixture.old_source
        ));
        b1::target_missing(fixture, false);
        assert_eq!(
            dedupe::binding_context::observe_spawn_nonce_marker(CANARY_TMUX),
            dedupe::binding_context::SpawnNonceMarker::Known(
                fixture.context.execution_nonce.clone()
            )
        );
    } else if !matches!(before, BeforeInput::InPlaceClear) {
        assert!(!readiness.is_ready(CANARY_CHANNEL));
        assert!(!readiness.accepts(CANARY_CHANNEL));
    }
    let identities = b1::restore_instructions(
        fixture,
        vec![
            b1::instruction(CANARY_CHANNEL + 101, b1::Q1),
            b1::instruction(CANARY_CHANNEL + 102, b1::Q2),
        ],
    )
    .await;
    let deps = IntakeDeps {
        http: &api.http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &fixture.shared,
        token: "e51-isolated-test-bot",
    };
    let admission = admit_text_intake(&deps, &input()).await;
    assert!(
        admission.is_local_for_tests(),
        "actual false-resumed GatewayHost must admit the initial fresh input: {admission:?}"
    );
    assert_eq!(b1::queue_identities(fixture).await, identities);
    let (observed_tx, observed_rx) = mpsc::channel();
    let _observe = crate::services::discord::codex_absence::test_setup::install(
        CANARY_CHANNEL,
        Box::new(move |_| {
            crate::services::codex::actual_submit_tests::observe_absence_admission(Box::new(
                move |observation, token| {
                    let token = token.expect("actual provider token");
                    let exact_self = observation.is_some_and(|observed| {
                        observed.matches_token(token)
                            && observed.recheck(CANARY_CHANNEL, CANARY_TMUX)
                    });
                    let row =
                        crate::services::discord::inflight::load_inflight_state_read_only_result(
                            &ProviderKind::Codex,
                            CANARY_CHANNEL,
                        )
                        .unwrap()
                        .unwrap();
                    assert_eq!(row.provider, "codex");
                    assert_eq!(row.channel_id, CANARY_CHANNEL);
                    assert_eq!(row.tmux_session_name.as_deref(), Some(CANARY_TMUX));
                    assert_eq!(row.user_msg_id, CANARY_CHANNEL + 101);
                    assert_eq!(row.request_owner_user_id, 1);
                    assert_eq!(row.turn_nonce.as_deref(), token.turn_nonce());
                    observed_tx
                        .send((Arc::clone(token), row.turn_nonce.unwrap(), exact_self))
                        .unwrap();
                },
            ));
            Box::new(())
        }),
    );
    fixture.fake.take_calls();
    assert!(b1::kickoff(fixture, api).await);
    let first = fixture.turn_failure_guard().await;
    let first_token = fixture
        .shared
        .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
        .snapshot()
        .await
        .cancel_token
        .unwrap();
    let (actual_token, nonce, exact_self) =
        receive(&observed_rx, "actual provider ExactSelf row").await;
    assert!(
        exact_self,
        "the actual provider must receive ExactSelf, not NoPriorRow or None"
    );
    assert!(Arc::ptr_eq(&actual_token, &first_token));
    let snapshot = fixture
        .shared
        .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
        .snapshot()
        .await;
    assert_eq!(snapshot.active_turn_nonce.as_deref(), Some(nonce.as_str()));
    assert_eq!(
        snapshot.active_user_message_id.map(|id| id.get()),
        Some(CANARY_CHANNEL + 101)
    );
    assert_eq!(snapshot.active_request_owner.map(|id| id.get()), Some(1));
    let fresh = b1::wait_context_after(fixture, &fixture.context.execution_nonce).await;
    if old_io.is_none() {
        *old_io = Some(
            crate::services::codex::actual_submit_tests::source_io::Watch::start(
                &fixture.old_source.path,
            ),
        );
    }
    assert_eq!(fresh.launch_mode, "fresh");
    assert_eq!(fresh.source_policy.as_deref(), Some("verified"));
    assert!(fresh.expected_native_session_id.is_none());
    if matches!(before, BeforeInput::RestartAfterContext) {
        restart_phase::assert_proof(&fresh, false);
        assert!(!readiness.is_ready(CANARY_CHANNEL));
        assert!(!readiness.accepts(CANARY_CHANNEL));
        restart_phase::restart(fixture, output, readiness, handles, &fresh, false).await;
        restart_phase::assert_proof(&fresh, false);
        assert!(!readiness.is_ready(CANARY_CHANNEL));
        assert!(!readiness.accepts(CANARY_CHANNEL));
    }
    let argv: Vec<String> =
        serde_json::from_slice(&fs::read(fixture.root.path().join("fresh-argv.json")).unwrap())
            .unwrap();
    assert_eq!(argv.iter().filter(|arg| arg.contains(b1::Q1)).count(), 1);
    assert!(
        argv.iter()
            .all(|arg| !arg.contains(b1::Q2) && arg != "resume")
    );
    assert_eq!(b1::queue_identities(fixture).await, identities[1..]);
    let accepted = Arc::new(Mutex::new(Vec::new()));
    let input_calls = Arc::new(Mutex::new(Vec::new()));
    let (_warm, entered, release, boundary) =
        native_output::warm_probe(&fresh, Arc::clone(&accepted), Arc::clone(&input_calls));
    let path = b1::publish_named_source(fixture, &fresh, NEW_NATIVE_ID);
    let new_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(&path);
    if matches!(before, BeforeInput::RestartAfterProof) {
        restart_phase::assert_proof(&fresh, true);
        restart_phase::restart(fixture, output, readiness, handles, &fresh, false).await;
        restart_phase::assert_proof(&fresh, true);
    }
    native_output::terminal(&path, "new-clear-turn", FIRST_REPLY);
    b1::wait_completion(&first_token).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let snapshot = fixture
                .shared
                .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
                .snapshot()
                .await;
            let same = snapshot
                .cancel_token
                .as_ref()
                .is_some_and(|token| Arc::ptr_eq(token, &first_token));
            let selected = fixture
                .shared
                .core
                .lock()
                .await
                .sessions
                .get(&serenity::ChannelId::new(CANARY_CHANNEL))
                .and_then(|session| session.session_id.as_deref())
                .map(str::to_owned);
            if !same && selected.as_deref() == Some(NEW_NATIVE_ID) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Q1 must release its exact token and publish the real native selector");
    eventually(
        || readiness.is_ready(CANARY_CHANNEL) && readiness.accepts(CANARY_CHANNEL),
        "scoped FullResume after actual proof",
    )
    .await;
    eventually(
        || {
            output
                .texts()
                .windows(2)
                .any(|pair| pair[0].contains(FIRST_REPLY) && pair[1] == "-# ✅ 완료")
        },
        "actual O first HTTP POST",
    )
    .await;
    first.finish();
    if matches!(before, BeforeInput::RestartAfterFirstPost) {
        restart_phase::assert_proof(&fresh, true);
        restart_phase::restart(fixture, output, readiness, handles, &fresh, true).await;
        restart_phase::assert_proof(&fresh, true);
    }
    crate::services::discord::turn_presence::activity::tests::companion_attach_log_watcher(
        &fixture.shared,
        CANARY_CHANNEL,
        CANARY_TMUX,
        &path,
    );
    let settled =
        crate::services::discord::turn_presence::activity::tests::companion_settled_activity(
            &fixture.shared,
            CANARY_CHANNEL,
        )
        .await;
    assert_eq!(
        settled.activity,
        crate::services::discord::turn_presence::activity::Activity::Idle
    );
    let prompt = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(prompt) = entered.try_recv() {
                break prompt;
            }
            let snapshot = fixture
                .shared
                .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
                .snapshot()
                .await;
            if snapshot.cancel_token.is_none() && !snapshot.intervention_queue.is_empty() {
                b1::kickoff(fixture, api).await;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Q2 must reach actual warm provider");
    assert!(prompt.contains(b1::Q2) && !prompt.contains(b1::Q1));
    let second = fixture.turn_failure_guard().await;
    let second_token = fixture
        .shared
        .mailbox(serenity::ChannelId::new(CANARY_CHANNEL))
        .snapshot()
        .await
        .cancel_token
        .unwrap();
    release.send(()).unwrap();
    eventually(
        || {
            if let Ok(stage) = boundary.try_recv() {
                panic!("actual warm provider held: {stage}");
            }
            accepted.lock().unwrap().len() == 1
        },
        "actual warm Enter",
    )
    .await;
    b1::wait_completion(&second_token).await;
    b1::wait_idle(fixture).await;
    eventually(
        || {
            output
                .texts()
                .windows(2)
                .any(|pair| pair[0].contains(SECOND_REPLY) && pair[1] == "-# ✅ 완료")
        },
        "actual O second HTTP POST",
    )
    .await;
    second.finish();
    assert!(b1::queue_identities(fixture).await.is_empty());
    assert_eq!(
        input_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.as_str() == "keys:Enter")
            .count(),
        1
    );
    let calls = fixture.fake.take_calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("new-session "))
            .count(),
        1,
        "{calls:?}"
    );
    assert!(
        calls.iter().all(|call| !call.starts_with("kill-session")),
        "{calls:?}"
    );
    assert_eq!(
        dedupe::binding_context::observe_spawn_nonce_marker(CANARY_TMUX),
        dedupe::binding_context::SpawnNonceMarker::Known(fresh.execution_nonce.clone())
    );
    assert_eq!(
        old_io.as_ref().unwrap().stats(),
        Default::default(),
        "suspended native body and cursor must remain untouched"
    );
    assert!(
        new_io.stats().read_bytes > 0,
        "actual fresh consumer must exercise the IO oracle"
    );
    let (disk, _) = crate::services::turn_orchestrator::pending_queue_persistence::load_channel_pending_queue_checked(
        &ProviderKind::Codex,
        &fixture.shared.token_hash,
        serenity::ChannelId::new(CANARY_CHANNEL),
    )
    .unwrap();
    assert!(disk.is_empty());
    fresh
}
