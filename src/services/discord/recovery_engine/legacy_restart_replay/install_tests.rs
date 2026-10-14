use super::super::super::manual_rebind::{
    coordinate_adoption::verified_restore, watcher_claim::claim_verified,
};
use super::super::reader_tests::{records, write};
use super::*;
use crate::services::discord::{self, TmuxWatcherHandle, tmux, tmux_watcher_now_ms};
use serenity::model::id::ChannelId;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const CHILD: &str = "ADK_5925_S1B_INSTALL_CHILD";
const CHANNEL: u64 = 5925120;
const FROZEN: u64 = 81001;
const BODY: u64 = 81002;
const PREFIX: &str = "FROZEN\nBODY 가나다\n";

fn qualified(name: &str) -> String {
    format!("{}::{name}", module_path!().split_once("::").unwrap().1)
}

#[cfg(unix)]
fn child(root: &Path, phase: &str, test: &str) -> std::process::Output {
    use std::os::unix::fs::PermissionsExt;
    let fake = root.join("tmux");
    std::fs::write(&fake,format!("#!/bin/sh\ncase \"$*\" in *kill-session*|*send-keys*) printf '%s\\n' \"$*\" >> '{}'; exit 97;; esac\ncase \"$*\" in *has-session*) exit 0;; *list-panes*) echo 0;; *capture-pane*) echo '›';; esac\n",root.join("forbidden-tmux").display())).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &qualified(test), "--nocapture"])
        .env(CHILD, phase)
        .env("AGENTDESK_ROOT_DIR", root)
        .env("TMPDIR", root)
        .env("PATH", root)
        .env("AGENTDESK_STATUS_INTERVAL_SECS", "0")
        .env(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[]",
        )
        .env_remove("DATABASE_URL")
        .output()
        .unwrap()
}

#[derive(Default)]
struct Recorder {
    messages: std::collections::BTreeMap<u64, String>,
    calls: Vec<(String, u64, String)>,
    next: u64,
}

async fn recorder() -> (
    Arc<Mutex<Recorder>>,
    Arc<serenity::http::Http>,
    tokio::task::JoinHandle<()>,
) {
    use axum::{
        Json,
        body::Bytes,
        http::{Method, StatusCode, Uri},
        response::IntoResponse,
    };
    let state = Arc::new(Mutex::new(Recorder {
        messages: [(FROZEN, "FROZEN\n".into()), (BODY, "BODY 가나다\n".into())].into(),
        next: 82000,
        ..Recorder::default()
    }));
    let captured = state.clone();
    let app=axum::Router::new().fallback(axum::routing::any(move |method:Method,uri:Uri,bytes:Bytes| {
        let state=captured.clone();
        async move {
            let mut r=state.lock().unwrap();
            let id=uri.path().rsplit('/').next().and_then(|v|v.parse::<u64>().ok()).unwrap_or_else(|| {r.next+=1;r.next});
            let body=serde_json::from_slice::<serde_json::Value>(&bytes).ok().and_then(|v|v.get("content").and_then(|v|v.as_str()).map(str::to_owned)).unwrap_or_default();
            r.calls.push((method.to_string(),id,body.clone()));
            std::fs::write(PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap()).join("http-calls.json"),serde_json::to_vec_pretty(&r.calls).unwrap()).unwrap();
            if method==Method::DELETE {r.messages.remove(&id);return StatusCode::NO_CONTENT.into_response();}
            if method==Method::PATCH || method==Method::POST {r.messages.insert(id,body.clone());}
            if body.contains("TAIL") {
                let phase=std::env::var(CHILD).unwrap_or_default();
                if phase=="MalformedReply" {return Json(serde_json::json!({"ack":"lost"})).into_response();}
                if phase=="UnconfirmedSourceChange" {
                    let row=inflight::load_inflight_state_read_only(&ProviderKind::Codex,CHANNEL).unwrap();
                    let marker=crate::services::tmux_common::session_temp_path(row.tmux_session_name.as_deref().unwrap(),"generation");
                    let replacement=PathBuf::from(format!("{marker}.fault"));
                    std::fs::write(&replacement,b"changed after transport").unwrap();
                    std::fs::rename(replacement,marker).unwrap();
                }
            }
            Json(serde_json::json!({"id":id.to_string(),"channel_id":CHANNEL.to_string(),"content":r.messages.get(&id).cloned().unwrap_or_default(),
                "author":{"id":"1","username":"fixture","discriminator":"0001","avatar":null},"timestamp":"2026-10-14T00:00:00Z","edited_timestamp":null,
                "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0})).into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        serenity::all::HttpBuilder::new("fixture")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (state, http, server)
}

fn initial(root: &Path, long: bool) -> inflight::InflightTurnState {
    let raw = root.join("raw.jsonl");
    let mut source = records();
    if long {
        source[7] = super::super::reader_tests::assistant(&format!("TAIL\n{}", "x".repeat(2300)));
    }
    write(&raw, &source);
    let session = ProviderKind::Codex.build_tmux_session_name("p5925-s1b-fixture");
    let mut row = inflight::InflightTurnState::new(
        ProviderKind::Codex,
        CHANNEL,
        None,
        7,
        11,
        BODY,
        "fixture".into(),
        Some("s1".into()),
        Some(session),
        Some(root.join("lost.jsonl").to_string_lossy().into()),
        None,
        0,
    );
    row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui);
    row.native_turn_id = Some("cur".into());
    row.turn_nonce = Some("nonce".into());
    row.turn_start_offset = Some(128);
    row.last_offset = 4096;
    row.full_response = PREFIX.into();
    row.response_sent_offset = "FROZEN\n".len();
    row.streaming_rollover_frozen_msg_ids = vec![FROZEN];
    row.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
    use discord::outbound::delivery_record as dr;
    let record_path = dr::delivery_record_path(&ProviderKind::Codex, CHANNEL).unwrap();
    std::fs::create_dir_all(record_path.parent().unwrap()).unwrap();
    let prior = dr::DeliveryRecord {
        confirmed_deliveries: vec![dr::ConfirmedDeliveryReceipt {
            source: dr::ExactJsonlSourceIdentity {
                provider: "codex".into(),
                tmux_session_name: row.tmux_session_name.clone().unwrap(),
                turn_nonce: "previous-source".into(),
                range: (0, 128),
                generation_mtime_ns: 77,
                offset_authority_channel_id: CHANNEL,
                delivery_channel_id: CHANNEL,
            },
            delivery_channel_id: CHANNEL,
            message_id: FROZEN,
        }],
        ..dr::DeliveryRecord::default()
    };
    std::fs::write(record_path, serde_json::to_vec_pretty(&prior).unwrap()).unwrap();
    inflight::save_inflight_state(&row).unwrap();
    inflight::load_inflight_state(&ProviderKind::Codex, CHANNEL).unwrap()
}

#[cfg(unix)]
async fn run_child(phase: &str) {
    let root = PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    let snapshot = inflight::load_inflight_state_read_only(&ProviderKind::Codex, CHANNEL)
        .unwrap_or_else(|| initial(&root, phase == "Long"));
    let mut shared = discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Codex;
    let (observed, http, server) = recorder().await;
    let raw = root.join("raw.jsonl");
    let complete_raw = std::fs::read(&raw).unwrap();
    let prior_receipts =
        discord::outbound::delivery_record::read_record(&ProviderKind::Codex, CHANNEL)
            .unwrap()
            .confirmed_deliveries;
    let prior_receipt_bytes = serde_json::to_vec(&prior_receipts).unwrap();
    if phase == "Partial" {
        let split = std::str::from_utf8(&complete_raw)
            .unwrap()
            .find('가')
            .unwrap()
            + 1;
        std::fs::write(&raw, &complete_raw[..split]).unwrap();
    }
    let output = crate::services::tmux_common::session_temp_path(
        snapshot.tmux_session_name.as_deref().unwrap(),
        "jsonl",
    );
    let row_path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Codex,
        CHANNEL,
    );
    let row_backup = row_path.with_extension("fault-original");
    let thread_fault = (phase == "Thread")
        .then(super::super::super::rebind_runtime::codex_spawn::fail_reader_thread_for_test);
    let other_cancel = Arc::new(AtomicBool::new(false));
    if phase == "ForeignOwner" {
        shared.tmux_watchers.insert(
            ChannelId::new(CHANNEL + 1),
            TmuxWatcherHandle {
                tmux_session_name: snapshot.tmux_session_name.clone().unwrap(),
                output_path: output.clone(),
                paused: Arc::new(AtomicBool::new(false)),
                resume_offset: Arc::new(Mutex::new(None)),
                cancel: other_cancel.clone(),
                pause_epoch: Arc::new(AtomicU64::new(0)),
                turn_delivered: Arc::new(AtomicBool::new(false)),
                last_heartbeat_ts_ms: Arc::new(AtomicI64::new(tmux_watcher_now_ms())),
            },
        );
    }
    if phase == "ForeignOwner" {
        std::fs::write(&output, b"FOREIGN WRITER BYTES").unwrap();
    }
    let observation = inflight::o_seed_observation::Guard::new(&ProviderKind::Codex, CHANNEL);
    let mut seen = Vec::new();
    let mut checkpoint = |point: InstallCheckpoint| {
        seen.push(point);
        if [
            InstallCheckpoint::WriterClaimPrepared,
            InstallCheckpoint::HoldReleased,
        ]
        .contains(&point)
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert!(
                std::fs::read(&output).unwrap().is_empty(),
                "writer ran before gate"
            );
            assert!(
                !observation.snapshot().watcher_initialization_observed,
                "watcher ran before gate"
            );
        }
        let calls = observed.lock().unwrap().calls.clone();
        assert!(
            calls
                .iter()
                .all(|(method, _, _)| method == "GET" || method == "HEAD"),
            "external effect before gate at {point:?}: {calls:?}"
        );
        if phase == format!("{point:?}") {
            std::process::exit(87);
        }
        if [
            "Raw",
            "Truncate",
            "Swap",
            "RawLate",
            "TruncateLate",
            "SwapLate",
        ]
        .contains(&phase)
            && point
                == if phase.ends_with("Late") {
                    InstallCheckpoint::CoordinateInstalled
                } else {
                    InstallCheckpoint::TemporaryPrepared
                }
        {
            if phase.starts_with("Swap") {
                std::fs::rename(&raw, root.join("old-raw")).unwrap();
            }
            let changed = match phase {
                "Truncate" | "TruncateLate" => Vec::new(),
                "Raw" | "RawLate" => String::from_utf8(complete_raw.clone())
                    .unwrap()
                    .replace("FROZEN", "BROKEN")
                    .into_bytes(),
                _ => complete_raw.clone(),
            };
            std::fs::write(&raw, changed).unwrap();
        }
        if phase == "Drift" && point == InstallCheckpoint::TemporaryPrepared {
            let mut row =
                inflight::load_inflight_state_read_only(&ProviderKind::Codex, CHANNEL).unwrap();
            row.current_tool_line = Some("changed during proof".into());
            inflight::save_inflight_state(&row).unwrap();
        }
        if phase == "Claim" && point == InstallCheckpoint::CoordinateInstalled {
            shared.tmux_watchers.insert(
                ChannelId::new(CHANNEL + 1),
                TmuxWatcherHandle {
                    tmux_session_name: snapshot.tmux_session_name.clone().unwrap(),
                    output_path: output.clone(),
                    paused: Arc::new(AtomicBool::new(false)),
                    resume_offset: Arc::new(Mutex::new(None)),
                    cancel: other_cancel.clone(),
                    pause_epoch: Arc::new(AtomicU64::new(0)),
                    turn_delivered: Arc::new(AtomicBool::new(false)),
                    last_heartbeat_ts_ms: Arc::new(AtomicI64::new(tmux_watcher_now_ms())),
                },
            );
        }
        if phase == "ReplacedClaim" && point == InstallCheckpoint::WriterClaimPrepared {
            let handle = {
                let old = shared.tmux_watchers.get(&ChannelId::new(CHANNEL)).unwrap();
                TmuxWatcherHandle {
                    tmux_session_name: old.tmux_session_name.clone(),
                    output_path: old.output_path.clone(),
                    paused: old.paused.clone(),
                    resume_offset: old.resume_offset.clone(),
                    cancel: other_cancel.clone(),
                    pause_epoch: old.pause_epoch.clone(),
                    turn_delivered: old.turn_delivered.clone(),
                    last_heartbeat_ts_ms: old.last_heartbeat_ts_ms.clone(),
                }
            };
            shared.tmux_watchers.insert(ChannelId::new(CHANNEL), handle);
        }
        if phase == "Rename" && point == InstallCheckpoint::TemporaryPrepared {
            std::fs::create_dir(&output).unwrap();
        }
        if phase == "Persist" && point == InstallCheckpoint::WriterClaimPrepared {
            std::fs::rename(&row_path, &row_backup).unwrap();
            std::fs::create_dir(&row_path).unwrap();
        }
        if phase == "Gate" && point == InstallCheckpoint::HoldReleased {
            let marker = crate::services::tmux_common::session_temp_path(
                snapshot.tmux_session_name.as_deref().unwrap(),
                "generation",
            );
            std::fs::write(root.join("changed-generation"), b"another incarnation").unwrap();
            std::fs::rename(root.join("changed-generation"), marker).unwrap();
        }
        Ok(())
    };
    let installed = install(
        InstallRequest {
            shared: &shared,
            snapshot: &snapshot,
            raw_path: &root.join("raw.jsonl"),
            source_policy: SourcePolicy::Legacy,
        },
        &mut checkpoint,
        |locked, writer, finish| {
            let restored = verified_restore(locked, finish).map_err(|e| format!("{e:?}"))?;
            assert!(restored.same_turn_rewind);
            let mut expected = tmux::restored_watcher_turn_from_inflight(
                locked.state(),
                locked.state().tmux_session_name.as_deref().unwrap(),
                finish,
            )
            .unwrap();
            expected.same_turn_rewind = true;
            assert_eq!(format!("{restored:?}"), format!("{expected:?}"));
            let session = locked.state().tmux_session_name.clone().unwrap();
            let path = locked.state().output_path.clone().unwrap();
            let (paused, resume, epoch, delivered, beat) = (
                Arc::new(AtomicBool::new(false)),
                Arc::new(Mutex::new(None)),
                Arc::new(AtomicU64::new(0)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicI64::new(tmux_watcher_now_ms())),
            );
            let handle = TmuxWatcherHandle {
                tmux_session_name: session.clone(),
                output_path: path.clone(),
                paused: paused.clone(),
                resume_offset: resume.clone(),
                cancel: writer.cancel.clone(),
                pause_epoch: epoch.clone(),
                turn_delivered: delivered.clone(),
                last_heartbeat_ts_ms: beat.clone(),
            };
            let run = tokio::runtime::Handle::current();
            let own_shared = shared.clone();
            let http = http.clone();
            let cancel = writer.cancel.clone();
            let await_terminal = ["MalformedReply", "UnconfirmedSourceChange"].contains(&phase);
            claim_verified(
                &shared.tmux_watchers,
                ChannelId::new(CHANNEL),
                handle,
                None,
                writer.gate.clone(),
                move || {
                    run.block_on(Box::pin(async move {
                        if await_terminal {
                            loop {
                                let ready =
                                    std::fs::read_to_string(&path).ok().is_some_and(|bytes| {
                                        bytes.ends_with('\n')
                                            && bytes.lines().any(|line| {
                                                serde_json::from_str::<serde_json::Value>(line)
                                                    .is_ok_and(|value| {
                                                        value
                                                            .get("type")
                                                            .and_then(serde_json::Value::as_str)
                                                            == Some("result")
                                                    })
                                            })
                                    });
                                if ready {
                                    break;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                            }
                        }
                        tmux::tmux_output_watcher_with_restore(
                            ChannelId::new(CHANNEL),
                            http,
                            own_shared,
                            path,
                            session,
                            0,
                            cancel,
                            paused,
                            resume,
                            epoch,
                            delivered,
                            beat,
                            Some(restored),
                        )
                        .await;
                    }));
                    Ok(())
                },
            )
        },
    )
    .await;
    if [
        "Partial",
        "Rename",
        "Persist",
        "Thread",
        "Claim",
        "Gate",
        "Raw",
        "Truncate",
        "Swap",
        "RawLate",
        "TruncateLate",
        "SwapLate",
        "Drift",
        "ReplacedClaim",
        "ForeignOwner",
    ]
    .contains(&phase)
    {
        assert!(
            installed.is_err(),
            "fault {phase} was hidden as installation success"
        );
        let calls = observed.lock().unwrap().calls.clone();
        assert!(
            calls
                .iter()
                .all(|(method, _, _)| method == "GET" || method == "HEAD"),
            "effect during fault {phase}: {calls:?}"
        );
        assert!(!other_cancel.load(Ordering::Acquire));
        if phase == "Persist" {
            std::fs::remove_dir(&row_path).unwrap();
            std::fs::rename(&row_backup, &row_path).unwrap();
        }
        if phase == "Rename" {
            std::fs::remove_dir(&output).unwrap();
        }
        if [
            "Partial",
            "Raw",
            "Truncate",
            "Swap",
            "RawLate",
            "TruncateLate",
            "SwapLate",
        ]
        .contains(&phase)
        {
            std::fs::write(&raw, &complete_raw).unwrap();
        }
        drop(thread_fault);
        let durable =
            inflight::load_inflight_state_read_only(&ProviderKind::Codex, CHANNEL).unwrap();
        assert!(
            durable
                .replay_hold_reasons
                .iter()
                .any(|h| h == "p5925_coordinate:pending")
        );
        if [
            "Persist",
            "Thread",
            "Claim",
            "Gate",
            "RawLate",
            "TruncateLate",
            "SwapLate",
            "ReplacedClaim",
        ]
        .contains(&phase)
        {
            assert_eq!(durable.last_offset, 0);
        }
        if phase == "ForeignOwner" {
            assert_eq!(std::fs::read(&output).unwrap(), b"FOREIGN WRITER BYTES");
            assert_eq!(durable.last_offset, snapshot.last_offset);
            std::fs::remove_file(&output).unwrap();
        }
        server.abort();
        drop(observation);
        Box::pin(run_child("Retry")).await;
        return;
    }
    let installed = installed.unwrap_or_else(|e| panic!("installer {phase}: {e:?}"));
    assert_eq!(seen.len(), 7);
    assert_eq!(installed.state.last_offset, 0);
    assert_eq!(installed.state.turn_start_offset, Some(0));
    assert_eq!(installed.state.last_watcher_relayed_offset, None);
    assert_eq!(
        installed.state.last_watcher_relayed_generation_mtime_ns,
        None
    );
    for field in [
        "provider",
        "channel_id",
        "request_owner_user_id",
        "user_msg_id",
        "started_at",
        "born_generation",
        "finalizer_turn_id",
        "session_id",
        "native_turn_id",
        "full_response",
        "rebind_origin",
    ] {
        let actual = serde_json::to_value(&installed.state).unwrap();
        let prior = serde_json::to_value(&snapshot).unwrap();
        assert!(
            actual.get(field).is_some() && prior.get(field).is_some(),
            "missing identity field {field}"
        );
        assert_eq!(
            actual[field], prior[field],
            "identity field {field} changed"
        );
    }
    assert_eq!(installed.state.current_msg_id, snapshot.current_msg_id);
    assert_eq!(
        installed.state.streaming_rollover_frozen_msg_ids,
        snapshot.streaming_rollover_frozen_msg_ids
    );
    assert_eq!(
        installed.state.response_sent_offset,
        snapshot.response_sent_offset
    );
    assert_eq!(installed.state.turn_nonce, snapshot.turn_nonce);
    assert_eq!(
        installed.state.effective_relay_owner_kind(),
        snapshot.effective_relay_owner_kind()
    );
    assert!(
        !installed
            .state
            .replay_hold_reasons
            .iter()
            .any(|h| h.starts_with("p5925_coordinate:"))
    );
    tokio::task::spawn_blocking(move || installed.writer.join())
        .await
        .unwrap()
        .unwrap();
    if ["MalformedReply", "UnconfirmedSourceChange"].contains(&phase) {
        tokio::time::timeout(std::time::Duration::from_secs(8), async {
            loop {
                if observed
                    .lock()
                    .unwrap()
                    .calls
                    .iter()
                    .any(|(m, _, b)| (m == "PATCH" || m == "POST") && b.contains("TAIL"))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        installed.claim.stop();
        tokio::task::block_in_place(move || installed.claim.join()).unwrap();
        let coord = shared.tmux_relay_coord(ChannelId::new(CHANNEL));
        assert_eq!(coord.confirmed_end_offset.load(Ordering::Acquire), 0);
        let record =
            discord::outbound::delivery_record::read_record(&ProviderKind::Codex, CHANNEL).unwrap();
        assert_eq!(
            serde_json::to_vec(&record.confirmed_deliveries).unwrap(),
            prior_receipt_bytes
        );
        assert!(record.delivered_frontier.is_none());
        let durable =
            inflight::load_inflight_state_read_only(&ProviderKind::Codex, CHANNEL).unwrap();
        assert_eq!(durable.turn_nonce, snapshot.turn_nonce);
        assert_eq!(durable.current_msg_id, snapshot.current_msg_id);
        assert_eq!(
            durable.streaming_rollover_frozen_msg_ids,
            snapshot.streaming_rollover_frozen_msg_ids
        );
        assert_eq!(durable.response_sent_offset, snapshot.response_sent_offset);
        assert_eq!(durable.last_offset, 0);
        assert!(!durable.terminal_delivery_committed);
        assert_eq!(durable.full_response, PREFIX);
        server.abort();
        return;
    }
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            if observed.lock().unwrap().messages.values().any(|s| {
                s.contains("BODY 가나다")
                    && s.contains("TAIL")
                    && !s
                        .chars()
                        .next()
                        .is_some_and(|c| ('\u{2800}'..='\u{28ff}').contains(&c))
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|e| {
        panic!(
            "complete evidence must visibly deliver TAIL: {e:?}, calls={:?}, messages={:?}",
            observed.lock().unwrap().calls,
            std::fs::read_to_string(root.join("http-calls.json"))
        )
    });
    installed.claim.stop();
    tokio::task::block_in_place(move || installed.claim.join()).unwrap();
    assert!(observation.snapshot().watcher_initialization_observed);
    let recorded = observed.lock().unwrap();
    assert_eq!(
        recorded
            .messages
            .values()
            .map(|s| s.matches("TAIL").count())
            .sum::<usize>(),
        1
    );
    assert!(recorded.messages.contains_key(&BODY));
    assert!(recorded.messages.contains_key(&FROZEN));
    assert!(
        !recorded
            .calls
            .iter()
            .any(|(method, _, body)| method == "POST"
                && (body.contains("OLD") || body.contains("FROZEN")))
    );
    let record =
        discord::outbound::delivery_record::read_record(&ProviderKind::Codex, CHANNEL).unwrap();
    assert_eq!(
        serde_json::to_vec(&record.confirmed_deliveries[..prior_receipts.len()]).unwrap(),
        prior_receipt_bytes
    );
    assert!(!root.join("forbidden-tmux").exists());
    server.abort();
}

#[cfg(unix)]
#[test]
fn seven_crash_boundaries_reenter_with_fresh_proof_and_visible_tail() {
    if let Ok(phase) = std::env::var(CHILD) {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_child(&phase));
        return;
    }
    for point in [
        InstallCheckpoint::PendingSaved,
        InstallCheckpoint::TemporaryPrepared,
        InstallCheckpoint::GenerationPublished,
        InstallCheckpoint::FileInstalled,
        InstallCheckpoint::CoordinateInstalled,
        InstallCheckpoint::WriterClaimPrepared,
        InstallCheckpoint::HoldReleased,
    ] {
        let root = tempfile::tempdir().unwrap();
        let a = child(
            root.path(),
            &format!("{point:?}"),
            "seven_crash_boundaries_reenter_with_fresh_proof_and_visible_tail",
        );
        assert_eq!(
            a.status.code(),
            Some(87),
            "A {point:?}: {} {}",
            String::from_utf8_lossy(&a.stdout),
            String::from_utf8_lossy(&a.stderr)
        );
        let b = child(
            root.path(),
            "Retry",
            "seven_crash_boundaries_reenter_with_fresh_proof_and_visible_tail",
        );
        assert!(
            b.status.success(),
            "B {point:?}: {} {}",
            String::from_utf8_lossy(&b.stdout),
            String::from_utf8_lossy(&b.stderr)
        );
        assert!(String::from_utf8_lossy(&b.stdout).contains("1 passed; 0 failed; 0 ignored"));
    }
}

#[cfg(unix)]
#[test]
fn body_patch_and_rollover_preserve_seed_ids_and_deliver_tail_once() {
    if let Ok(phase) = std::env::var(CHILD) {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_child(&phase));
        return;
    }
    for phase in ["Retry", "Long"] {
        let root = tempfile::tempdir().unwrap();
        let output = child(
            root.path(),
            phase,
            "body_patch_and_rollover_preserve_seed_ids_and_deliver_tail_once",
        );
        assert!(
            output.status.success(),
            "{phase}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed; 0 ignored"));
    }
}

#[cfg(unix)]
#[test]
fn partial_input_and_prepare_faults_retry_without_reinjecting_input_or_cancelling_other_owner() {
    if let Ok(phase) = std::env::var(CHILD) {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_child(&phase));
        return;
    }
    for phase in [
        "Partial",
        "Rename",
        "Persist",
        "Thread",
        "Claim",
        "Gate",
        "Raw",
        "Truncate",
        "Swap",
        "RawLate",
        "TruncateLate",
        "SwapLate",
        "Drift",
        "ReplacedClaim",
        "ForeignOwner",
    ] {
        let root = tempfile::tempdir().unwrap();
        let output = child(
            root.path(),
            phase,
            "partial_input_and_prepare_faults_retry_without_reinjecting_input_or_cancelling_other_owner",
        );
        assert!(
            output.status.success(),
            "{phase}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed; 0 ignored"));
    }
}

#[test]
fn rejected_durable_write_is_never_installation_success() {
    assert!(saved(inflight::GuardedSaveOutcome::AuthorityPinned).is_err());
    assert!(saved(inflight::GuardedSaveOutcome::IoError).is_err());
    assert!(saved(inflight::GuardedSaveOutcome::Saved).is_ok());
}

#[cfg(unix)]
#[test]
fn unconfirmed_transport_preserves_receipt_bytes_identity_and_frontier() {
    if let Ok(phase) = std::env::var(CHILD) {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_child(&phase));
        return;
    }
    for phase in ["MalformedReply", "UnconfirmedSourceChange"] {
        let root = tempfile::tempdir().unwrap();
        let result = child(
            root.path(),
            phase,
            "unconfirmed_transport_preserves_receipt_bytes_identity_and_frontier",
        );
        assert!(
            result.status.success(),
            "{phase}: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed; 0 failed; 0 ignored"));
    }
}

#[cfg(unix)]
#[test]
fn identity_clear_preserves_stale_source_and_clears_current_or_unmarked_rows() {
    use std::os::unix::fs::MetadataExt;
    let Ok(_) = std::env::var(CHILD) else {
        let root = tempfile::tempdir().unwrap();
        let result = child(
            root.path(),
            "Clear",
            "identity_clear_preserves_stale_source_and_clears_current_or_unmarked_rows",
        );
        assert!(
            result.status.success(),
            "{} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed; 0 failed; 0 ignored"));
        return;
    };
    let root = PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    let snapshot = initial(&root, false);
    let generation = crate::services::tmux_common::session_temp_path(
        snapshot.tmux_session_name.as_deref().unwrap(),
        "generation",
    );
    let output = root.join("normalized");
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Codex,
        CHANNEL,
    );
    for case in [
        "unmarked",
        "current",
        "stale",
        "file_swap",
        "preparing",
        "source_locked",
    ] {
        std::fs::write(&generation, case).unwrap();
        std::fs::write(&output, b"normalized bytes").unwrap();
        let metadata = std::fs::metadata(&output).unwrap();
        let mut value = serde_json::to_value(&snapshot).unwrap();
        value["output_path"] = serde_json::json!(output);
        if case != "unmarked" {
            value["legacy_replay_coordinate"] = serde_json::json!({
                "version":1,"install_id":"clear-source","phase":if case=="preparing" {"preparing"} else {"installed"},
                "prior":{"output_path":null,"last_offset":128,"turn_start_offset":128},
                "installed":{"output_path":output,"file_identity":[metadata.dev(),metadata.ino()],
                "generation_mtime_ns":discord::outbound::delivery_record::current_generation_mtime_ns(snapshot.tmux_session_name.as_deref().unwrap())}
            });
        }
        let state: inflight::InflightTurnState = serde_json::from_value(value).unwrap();
        let bytes = serde_json::to_vec_pretty(&state).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        if case == "stale" {
            let replacement = root.join("replacement-generation");
            std::fs::write(&replacement, "new").unwrap();
            let previous = state.coordinate_source().unwrap().1.generation_mtime_ns;
            // Fast writes can share mtime; publish a distinct key as reconstruction does.
            let time = std::time::UNIX_EPOCH
                + std::time::Duration::from_nanos(previous as u64 + 1_000_000_000);
            std::fs::File::open(&replacement)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(time))
                .unwrap();
            std::fs::rename(&replacement, &generation).unwrap();
            assert_ne!(
                discord::outbound::delivery_record::current_generation_mtime_ns(
                    snapshot.tmux_session_name.as_deref().unwrap()
                ),
                previous,
                "fixture generation did not change"
            );
        }
        if case == "file_swap" {
            std::fs::write(root.join("replacement-output"), "new").unwrap();
            std::fs::rename(root.join("replacement-output"), &output).unwrap();
        }
        let clear = || {
            inflight::clear_inflight_state_if_matches_identity_turn_nonce(
                &ProviderKind::Codex,
                CHANNEL,
                &inflight::InflightTurnIdentity::from_state(&state),
                state.turn_nonce.as_deref(),
            )
        };
        let outcome = if case == "source_locked" {
            crate::services::tmux_common::with_tmux_source_authority(
                state.tmux_session_name.as_deref().unwrap(),
                |_| clear(),
            )
        } else {
            clear()
        };
        if ["unmarked", "current"].contains(&case) {
            assert_eq!(outcome, inflight::GuardedClearOutcome::Cleared, "{case}");
            assert!(!path.exists(), "{case}");
        } else {
            assert_eq!(
                std::fs::read(&path).ok().as_deref(),
                Some(bytes.as_slice()),
                "{case}: clear lost the durable obligation"
            );
            assert_eq!(outcome, inflight::GuardedClearOutcome::IoError, "{case}");
        }
    }
}
