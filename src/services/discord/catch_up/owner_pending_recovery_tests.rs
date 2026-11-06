//! A live arrival the input owner left Pending is recovered by the real catch-up sweep under the
//! same ticket and the same stored original, though catch-up renders the message differently.
use super::*;
use crate::services::discord::queue_io::input_ingress::{self, Intake};
use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};
use crate::services::discord::{IntakeRequest, TurnKind};

fn live_request(
    channel_id: ChannelId,
    message_id: MessageId,
    reply: Option<&str>,
) -> IntakeRequest {
    IntakeRequest {
        intake_outbox_id: None,
        channel_id,
        user_msg_id: message_id,
        source_message_ids: vec![],
        busy_followup_retry_user_msg_id: message_id,
        request_owner: serenity::UserId::new(HUMAN_ID),
        request_owner_name: "human".into(),
        user_text: format!("request {}", message_id.get()),
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        merge_consecutive: true,
        reply_context: reply.map(str::to_string),
        has_reply_boundary: reply.is_some(),
        dm_hint: None,
        turn_kind: TurnKind::Foreground,
        preserve_on_cancel: true,
    }
}

async fn snapshot(fx: &Fixture, channel_id: ChannelId) -> serde_json::Value {
    let handle = fx.shared.mailbox(channel_id);
    let read = handle.v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
    read.await.unwrap()
}

async fn recover(channel_seed: u64, reply: Option<&str>, uploads: Vec<&str>) {
    let fx = Fixture::new().await;
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "7").unwrap();
    let channel_id = ChannelId::new(channel_seed);
    let (checkpoint, q) = (id(1, 600), id(2, 150));
    fx.seed_checkpoint(channel_id, checkpoint);
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&fx.shared.token_hash)
        .join(format!("{channel_id}.json"));
    let handle = fx.shared.mailbox(channel_id);
    handle
        .v2_adopt(path.clone(), fx.shared.token_hash.clone())
        .await
        .unwrap();
    let request = live_request(channel_id, q, reply);
    let expected_uploads = uploads.len();
    let uploads = uploads.into_iter().map(Into::into).collect();
    let watch = Watch::new(
        &path,
        Plan {
            fail: vec![Step::ParentSync],
            ..Default::default()
        },
    );
    let intake = input_ingress::intake(&fx.shared, &fx.provider, &request, uploads, false, false);
    assert!(
        matches!(intake.await, Some(Intake::Pending(_))),
        "the live arrival is Pending"
    );
    drop(watch);
    let stored = snapshot(&fx, channel_id).await;
    assert!(
        stored["items"].as_array().unwrap().is_empty(),
        "nothing is committed yet"
    );

    let history = vec![own_reply(channel_id, checkpoint), human(channel_id, q)];
    let api = StrictApi::new(&fx.shared).with_history(channel_id, history);
    fx.sweep(&api).await;
    assert_eq!(
        api.enqueue_log(),
        vec![(q.get(), true, None)],
        "catch-up recovers it once"
    );
    let state = snapshot(&fx, channel_id).await;
    let items = state["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(state["admission_sequence"], 1);
    assert_eq!(state["admission_receipts"].as_array().unwrap().len(), 1);
    let envelope = items[0]["reply_context"].as_str().unwrap_or_default();
    assert!(
        envelope.starts_with('\u{1e}'),
        "the live original, not catch-up's copy: {envelope:?}"
    );
    if let Some(reply) = reply {
        assert!(envelope.contains(reply), "its reply context survives");
    }
    assert_eq!(
        items[0]["pending_uploads"].as_array().map_or(0, Vec::len),
        expected_uploads
    );
    fx.sweep(&api).await;
    assert_eq!(
        snapshot(&fx, channel_id).await["items"],
        state["items"],
        "a later sweep adds nothing"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_plain_live_pending_arrival_is_recovered_by_catch_up_as_its_stored_original() {
    recover(7_272_501, None, vec![]).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_live_pending_reply_with_an_upload_keeps_both_through_catch_up() {
    recover(7_272_502, Some("quoted line"), vec!["local asset ref"]).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn boot_live_pending_auto_reset_catch_up_commits_once_and_preserves_metadata() {
    use crate::services::discord::queue_io::input_owner::admission::{AdmissionState, status};
    let fx = Fixture::new().await;
    let channel = ChannelId::new(7_835_001);
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "7").unwrap();
    let path = write_v2_queue(&fx, channel, id(1, 700));
    let mut seed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    seed["items"] = serde_json::json!([]);
    seed["namespace"] = serde_json::json!(format!("claude/{}", fx.shared.token_hash));
    seed["terminal_identities"] = serde_json::json!([{
        "identity":{"primary":id(1, 700).get(),"queued_generation":7,
            "sources":[{"message_id":id(1, 700).get(),"queued_generation":7}]},
        "settled_at_ms":chrono::Utc::now().timestamp_millis() as u64}]);
    std::fs::write(&path, seed.to_string()).unwrap();
    let http = Arc::new(serenity::Http::new("unit-test-token"));
    let stale = crate::services::discord::runtime_bootstrap::restore_queued_and_inflight_for_tests(
        &http,
        &fx.shared,
        &fx.provider,
    )
    .await;
    assert!(stale.is_empty());
    let metadata = snapshot(&fx, channel).await["terminal_identities"].clone();
    assert_eq!(metadata, seed["terminal_identities"]);
    let (checkpoint, q) = (id(1, 600), id(2, 150));
    let request = live_request(channel, q, Some("original quoted line"));
    let watch = Watch::new(
        &path,
        Plan {
            fail: vec![Step::ParentSync],
            ..Default::default()
        },
    );
    let intake = input_ingress::intake(
        &fx.shared,
        &fx.provider,
        &request,
        vec!["original local upload".into()],
        false,
        false,
    )
    .await;
    let Some(Intake::Pending(pending)) = intake else {
        panic!("live intake must leave a stored original");
    };
    drop(watch);
    let before_reset = std::fs::read(&path).unwrap();
    let runtime_root = path.ancestors().nth(4).unwrap();
    let output = std::process::Command::new("python3")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "-c",
            r#"
import sys
import types
from pathlib import Path
# Reset does not parse scenarios; keep this bridge independent of the optional YAML parser.
sys.modules['yaml'] = types.ModuleType('yaml')
sys.path.insert(0, 'scripts/e2e')
from tui_relay.test_channel_reset import ResetServer, driver
with ResetServer(sys.argv[2], herdr=True) as api:
    driver.reset_channel_state(base_url=api.base_url, channel_id=api.channel,
                               runtime_root=Path(sys.argv[1]), provider='claude')
"#,
        ])
        .arg(runtime_root)
        .arg(channel.get().to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "reset failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before_reset,
        "reset must preserve the owner's exact snapshot"
    );
    fx.seed_checkpoint(channel, checkpoint);
    let api = StrictApi::new(&fx.shared).with_history(
        channel,
        vec![own_reply(channel, checkpoint), human(channel, q)],
    );
    fx.sweep(&api).await;
    let state = snapshot(&fx, channel).await;
    assert_eq!(
        status(&fx.shared.mailbox(channel), &pending.0).await,
        Ok(AdmissionState::Committed)
    );
    assert_eq!(state["admission_sequence"], 1);
    let receipts = state["admission_receipts"].as_array().unwrap();
    assert_eq!(receipts.len(), 1);
    let items = state["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["message_id"], q.get());
    assert!(
        items[0]["reply_context"]
            .as_str()
            .unwrap()
            .contains("original quoted line")
    );
    assert_eq!(
        items[0]["pending_uploads"],
        serde_json::json!(["original local upload"])
    );
    assert_eq!(state["terminal_identities"], metadata);
    assert_eq!(api.enqueue_log(), vec![(q.get(), true, None)]);
    fx.sweep(&api).await;
    assert_eq!(
        snapshot(&fx, channel).await,
        state,
        "a repeated catch-up cannot duplicate the original or ticket"
    );
}

/// A V2 queue file as a forward binary leaves it, holding `held` queued for a V2-capable binary.
fn write_v2_queue(fx: &Fixture, channel_id: ChannelId, held: MessageId) -> std::path::PathBuf {
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&fx.shared.token_hash)
        .join(format!("{channel_id}.json"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let item = serde_json::json!({"author_id":HUMAN_ID,"message_id":held.get(),"channel_id":channel_id.get(),
        "source_message_ids":[held.get()],"queued_generation":7,"text":format!("request {}", held.get())});
    let snapshot = serde_json::json!({"version":2,"items":[item],"head_injection":null,
        "retained_inputs":[],"terminal_identities":[],"namespace":fx.shared.token_hash});
    std::fs::write(&path, snapshot.to_string()).unwrap();
    path
}

/// On a binary that cannot write V2, the production claim never starts a legacy turn on a V2
/// channel, and the file stays byte for byte.
#[tokio::test(flavor = "current_thread")]
async fn a_v2_queue_channel_refuses_every_legacy_claim_and_keeps_its_file() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(7_272_503);
    let path = write_v2_queue(&fx, channel_id, id(2, 150));
    let before = std::fs::read(&path).unwrap();
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let owner = serenity::UserId::new(HUMAN_ID);
    let start = crate::services::discord::queue_io::mailbox_try_start_turn_behind_queue;
    assert!(!start(&fx.shared, channel_id, token, owner, id(3, 100), None).await);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

/// A message a V2 queue holds is known to the real catch-up sweep: it is not taken again, and the
/// frontier stays before it, as for any queued input this binary did not run.
#[tokio::test(flavor = "current_thread")]
async fn catch_up_never_retakes_a_message_a_v2_queue_holds() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(7_272_504);
    let (checkpoint, q) = (id(1, 600), id(2, 150));
    fx.seed_checkpoint(channel_id, checkpoint);
    let path = write_v2_queue(&fx, channel_id, q);
    let before = std::fs::read(&path).unwrap();
    let history = vec![own_reply(channel_id, checkpoint), human(channel_id, q)];
    let api = StrictApi::new(&fx.shared).with_history(channel_id, history);
    fx.sweep(&api).await;
    assert_eq!(
        api.enqueue_log(),
        Vec::<EnqueueRecord>::new(),
        "the V2 file keeps it"
    );
    assert_eq!(
        fx.surfaces(channel_id),
        (Some(checkpoint.get()), Some(checkpoint.get()))
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

/// The owner's retention of a terminal identity, as `tick` drops it.
const TERMINAL_RETENTION_MS: u64 = 86_400_000;

#[test]
fn existing_v2_boots_twice_and_commits_live_and_catch_up_inputs() {
    use sha2::{Digest, Sha256};
    let new_runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    };
    let mut runtime = new_runtime();
    let mut fx = runtime.block_on(Fixture::new());
    let logs = LogWriter::capture();
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "7").unwrap();
    let cases = ["empty", "queued", "retained", "unknown", "terminal"];
    let mut fixtures = Vec::new();
    let clock = || chrono::Utc::now().timestamp_millis().max(0) as u64;
    let settled = clock();
    for (index, case) in cases.iter().enumerate() {
        let channel = ChannelId::new(7_676_510 + index as u64);
        let held = id(1, 700).get();
        let original = serde_json::json!({"author_id":HUMAN_ID,"message_id":held,
            "channel_id":channel.get(),"source_message_ids":[held],
            "queued_generation":7,"text":"fixture input"});
        let identity = serde_json::json!({"primary":original["message_id"],"queued_generation":7,
            "sources":[{"message_id":original["message_id"],"queued_generation":7}]});
        let input = serde_json::json!({"identity":identity,"original":original,"phase":"unknown",
            "attempt":null,"payload":null,"confirmation_deadline_ms":null});
        let notice = serde_json::json!({"kind":"unconfirmed","enqueued":true});
        let mut state = serde_json::json!({"version":2,"items":[],"head_injection":null,
            "retained_inputs":[],"terminal_identities":[],
            "namespace":format!("claude/{}", fx.shared.token_hash)});
        match *case {
            "queued" => state["items"] = serde_json::json!([original]),
            "retained" => {
                state["retained_inputs"] = serde_json::json!([{
                "input":input,"reason":{"kind":"unconfirmed"},
                "notice":notice}])
            }
            "unknown" => {
                state["items"] = serde_json::json!([original]);
                state["head_injection"] = input;
            }
            "terminal" => {
                state["terminal_identities"] = serde_json::json!([{
                "identity":identity,"settled_at_ms":settled}])
            }
            _ => {}
        }
        fixtures.push((original, identity, notice));
        if *case == "empty" {
            let path = crate::services::discord::runtime_store::discord_pending_queue_root()
                .unwrap()
                .join("claude")
                .join(&fx.shared.token_hash)
                .join(format!("{channel}.json"));
            runtime
                .block_on(async {
                    fx.shared
                        .mailbox(channel)
                        .v2_ensure(path, format!("claude/{}", fx.shared.token_hash))
                        .await
                })
                .unwrap();
        } else {
            let path = write_v2_queue(&fx, channel, id(1, 700));
            std::fs::write(path, state.to_string()).unwrap();
        }
    }
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    for boot in 1..=2 {
        // Past the retention the boot rightly drops the terminal identity: that is no restore.
        let age = clock().saturating_sub(settled);
        assert!(
            age < TERMINAL_RETENTION_MS,
            "terminal fixture expired: {age} ms old at boot {boot}, past the 24 h retention"
        );
        runtime.block_on(async {
        let stale = crate::services::discord::runtime_bootstrap::restore_queued_and_inflight_for_tests(
            &http, &fx.shared, &fx.provider,
        ).await;
        assert!(stale.is_empty());
        for (index, case) in cases.iter().enumerate() {
            let channel = ChannelId::new(7_676_510 + index as u64);
            let handle = fx.shared.mailbox(channel);
            let before = snapshot(&fx, channel).await;
            let (original, identity, notice) = &fixtures[index];
            let assert_restored = |state: &serde_json::Value| {
                let items = state["items"].as_array().unwrap();
                let kept_original = match *case {
                    "queued" => {
                        let copies: Vec<_> = items.iter()
                            .filter(|item| item["message_id"] == original["message_id"]).collect();
                        assert_eq!(copies.len(), 1, "boot {boot} lost the queued fixture");
                        Some(copies[0])
                    }
                    "retained" | "unknown" => {
                        let copies: Vec<_> = state["retained_inputs"].as_array().unwrap().iter()
                            .filter(|entry| entry["input"]["identity"] == *identity).collect();
                        assert_eq!(copies.len(), 1, "boot {boot} lost the {case} fixture identity");
                        if *case == "retained" {
                            assert_eq!(copies[0]["notice"], *notice,
                                "boot {boot} changed the retained fixture notice");
                        } else {
                            assert!(state["head_injection"].is_null(),
                                "boot {boot} rearmed the unknown fixture");
                            assert!(items.iter().all(|item| item["message_id"] != original["message_id"]),
                                "boot {boot} requeued the unknown fixture");
                        }
                        Some(&copies[0]["input"]["original"])
                    }
                    "terminal" => {
                        assert_eq!(state["terminal_identities"].as_array().unwrap().iter()
                            .filter(|entry| entry["identity"] == *identity).count(), 1,
                            "boot {boot} lost the terminal fixture identity");
                        None
                    }
                    _ => None,
                };
                if let Some(kept) = kept_original {
                    for (field, value) in original.as_object().unwrap() {
                        assert_eq!(&kept[field], value,
                            "boot {boot} changed the {case} fixture's {field}");
                    }
                }
            };
            assert_restored(&before);
            let path = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap()
                .join("claude").join(&fx.shared.token_hash).join(format!("{channel}.json"));
            let expected = std::fs::read(&path).unwrap();
            // Repeating recovery in the same process must keep the actor already adopted.
            crate::services::discord::runtime_bootstrap::restore_queued_and_inflight_for_tests(
                &http, &fx.shared, &fx.provider,
            ).await;
            assert_restored(&snapshot(&fx, channel).await);
            assert!(handle.same_actor(&fx.shared.mailbox(channel)));
            let actual = std::fs::read(&path).unwrap();
            assert_eq!(actual, expected);
            let live = live_request(channel, id(boot * 10, 500), None);
            assert_eq!(input_ingress::intake(&fx.shared, &fx.provider, &live, vec![], false, false).await,
                Some(Intake::Accepted));
            let q = id(boot * 10 + 1, 150);
            let checkpoint = id(boot * 10, 300);
            fx.seed_checkpoint(channel, checkpoint);
            let api = StrictApi::new(&fx.shared).with_history(channel,
                vec![own_reply(channel, checkpoint), human(channel, q)]);
            fx.sweep(&api).await;
            assert_eq!(api.enqueue_log(), vec![(q.get(), true, None)]);
            let state = snapshot(&fx, channel).await;
            assert_restored(&state);
            assert_eq!(state["admission_sequence"], boot * 2);
            for message in [live.user_msg_id, q] {
                assert_eq!(state["items"].as_array().unwrap().iter()
                    .filter(|item| item["message_id"] == message.get()).count(), 1);
            }
            assert_eq!(state["retained_inputs"], before["retained_inputs"]);
            assert_eq!(state["terminal_identities"], before["terminal_identities"]);
            assert!(state["head_injection"].is_null());
            fx.sweep(&api).await;
            assert_eq!(snapshot(&fx, channel).await, state);
            assert_eq!(fx.surfaces(channel), (Some(q.get()), Some(q.get())),
                "catch-up advances only after its input commits");
            eprintln!("REPRO_A boot={boot} case={case} expected_sha256={} actual_sha256={} committed_sha256={} writer=Owner::commit admission_sequence={}",
                hex::encode(Sha256::digest(&expected)), hex::encode(Sha256::digest(actual)),
                hex::encode(Sha256::digest(std::fs::read(path).unwrap())), state["admission_sequence"]);
        }
        for index in 0..cases.len() {
            fx.shared.mailboxes.remove_fixture_for_test(ChannelId::new(7_676_510 + index as u64));
        }
        });
        // End the first process's actors and drives before the second boot adopts their files.
        drop(runtime);
        runtime = new_runtime();
        fx.shared = runtime.block_on(async {
            let shared = crate::services::discord::make_shared_data_for_tests();
            {
                let mut settings = shared.settings.write().await;
                settings.owner_user_id = Some(OWNER_ID);
                settings.allow_all_users = true;
            }
            shared
        });
    }
    let captured = logs.finish();
    let diagnostics: Vec<_> = captured
        .lines()
        .filter(|line| line.contains("queued input owner snapshot observation"))
        .collect();
    assert_eq!(
        diagnostics.len(),
        10,
        "one adoption per channel and process"
    );
    assert!(
        diagnostics
            .iter()
            .all(|line| line.contains("observation=\"adopt\""))
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|line| line.contains("expected_present=false"))
            .count(),
        1
    );
    let incarnations: HashSet<_> = diagnostics
        .iter()
        .map(|line| {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix("owner_incarnation="))
                .unwrap()
        })
        .collect();
    assert_eq!(incarnations.len(), 10);
    for line in diagnostics {
        eprintln!("{line}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn outside_owner_commits_and_resets_refuse_stale_intake() {
    use crate::services::turn_orchestrator::ChannelMailboxRegistry;
    use sha2::{Digest, Sha256};
    let fx = Fixture::new().await;
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "7").unwrap();
    for reset in [false, true] {
        let logs = LogWriter::capture();
        let channel = ChannelId::new(if reset { 7_676_521 } else { 7_676_520 });
        let path = write_v2_queue(&fx, channel, id(1, 700));
        let a = fx.shared.mailbox(channel);
        a.v2_adopt(path.clone(), fx.shared.token_hash.clone())
            .await
            .unwrap();
        let expected = std::fs::read(&path).unwrap();
        if reset {
            std::fs::write(&path, "[]").unwrap();
        } else {
            let b = ChannelMailboxRegistry::default();
            let b = b.handle(channel);
            assert!(!a.same_actor(&b));
            b.v2_adopt(path.clone(), fx.shared.token_hash.clone())
                .await
                .unwrap();
            let original = input_ingress::original(
                &fx.provider,
                channel,
                None,
                &queued_intervention(id(2, 500), 0),
            )
            .unwrap();
            let admitted = original.admit(&b).await;
            assert!(
                matches!(
                    admitted,
                    crate::services::discord::queue_io::input_owner::admission::Admission::Accepted(
                        _
                    )
                ),
                "{admitted:?}"
            );
        }
        let actual = std::fs::read(&path).unwrap();
        assert_ne!(actual, expected);
        let request = live_request(channel, id(3, 150), None);
        let refused =
            input_ingress::intake(&fx.shared, &fx.provider, &request, vec![], false, false).await;
        assert!(
            matches!(refused, Some(Intake::Rejected(ref error))
            if error.contains("snapshot changed outside owner")),
            "{refused:?}"
        );
        let (checkpoint, q) = (id(4, 300), id(5, 150));
        fx.seed_checkpoint(channel, checkpoint);
        let api = StrictApi::new(&fx.shared).with_history(
            channel,
            vec![own_reply(channel, checkpoint), human(channel, q)],
        );
        fx.sweep(&api).await;
        assert_eq!(api.enqueue_log(), vec![(q.get(), false, None); 2]);
        assert_eq!(
            fx.surfaces(channel),
            (Some(checkpoint.get()), Some(checkpoint.get()))
        );
        assert_eq!(std::fs::read(&path).unwrap(), actual);
        let captured = logs.finish();
        let diagnostics: Vec<_> = captured
            .lines()
            .filter(|line| line.contains("queued input owner snapshot observation"))
            .collect();
        let adoptions: Vec<_> = diagnostics
            .iter()
            .filter(|line| line.contains("observation=\"adopt\""))
            .map(|line| {
                line.split_whitespace()
                    .find_map(|field| field.strip_prefix("owner_incarnation="))
                    .unwrap()
            })
            .collect();
        assert_eq!(adoptions.len(), if reset { 1 } else { 2 });
        assert_eq!(
            adoptions.iter().collect::<HashSet<_>>().len(),
            adoptions.len()
        );
        let conflicts: Vec<_> = diagnostics
            .iter()
            .filter(|line| line.contains("observation=\"cas_mismatch\""))
            .collect();
        assert_eq!(
            conflicts.len(),
            3,
            "live and both catch-up phases refuse the stale owner"
        );
        for line in conflicts {
            let fields: HashMap<_, _> = line
                .split_whitespace()
                .filter_map(|field| field.split_once('='))
                .collect();
            assert_eq!(
                fields["namespace_sha256"],
                hex::encode(Sha256::digest(fx.shared.token_hash.as_bytes()))
            );
            assert_eq!(
                fields["path_sha256"],
                hex::encode(Sha256::digest(path.as_os_str().as_encoded_bytes()))
            );
            assert_eq!(fields["owner_pid"], std::process::id().to_string());
            assert_eq!(fields["owner_incarnation"], adoptions[0]);
            uuid::Uuid::parse_str(fields["owner_incarnation"]).unwrap();
            assert!(fields["last_success_sequence"].parse::<u64>().unwrap() > 0);
            assert_eq!(
                fields["expected_sha256"],
                hex::encode(Sha256::digest(&expected))
            );
            assert_eq!(
                fields["actual_sha256"],
                hex::encode(Sha256::digest(&actual))
            );
            assert_eq!(fields["candidate_sha256"].len(), 64);
            assert!(
                fields["candidate_sha256"]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            );
            assert_eq!(fields["expected_bytes"], expected.len().to_string());
            assert_eq!(fields["actual_bytes"], actual.len().to_string());
            assert!(fields["candidate_bytes"].parse::<usize>().unwrap() > 0);
            assert!(!line.contains(path.to_str().unwrap()));
            assert!(!line.contains(&fx.shared.token_hash));
            assert!(!line.contains("already queued") && !line.contains("request "));
        }
        for line in diagnostics {
            eprintln!("{line}");
        }
        eprintln!(
            "REPRO_{} expected_sha256={} actual_sha256={} writer={}",
            if reset { "C" } else { "B" },
            hex::encode(Sha256::digest(expected)),
            hex::encode(Sha256::digest(actual)),
            if reset {
                "test std::fs::write([])"
            } else {
                "registry B Owner::commit"
            }
        );
    }
}
