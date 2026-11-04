use super::*;
use std::io::Write;
use std::sync::Mutex;

#[derive(Clone)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedLogs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn s2a_actual_boot_reports_quarantined_channels_with_no_restorable_queue() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let shared = {
        let _entered = runtime.enter();
        crate::services::discord::make_shared_data_for_tests()
    };
    let provider = ProviderKind::Claude;
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join(provider.as_str())
        .join(&shared.token_hash);
    std::fs::create_dir_all(&dir).unwrap();
    let cases = [
        (6_813_261_u64, "[{"),
        (6_813_262, "{\"version\":1,\"items\":["),
        (6_813_263, "{\"version\":2,\"items\":[]}"),
    ];
    for (channel, body) in cases {
        std::fs::write(dir.join(format!("{channel}.json")), body).unwrap();
    }
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = CapturedLogs(logs.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    tracing::subscriber::with_default(subscriber, || {
        runtime.block_on(async {
            let stale_cards = restore_queued_and_inflight_work(&http, &shared, &provider).await;
            assert!(stale_cards.is_empty());
            assert!(shared.mailboxes.snapshot_all().await.is_empty());
        });
    });
    let captured = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    let summaries: Vec<_> = captured
        .lines()
        .filter(|line| line.contains("pending queue recovery kept quarantined channels"))
        .collect();
    assert_eq!(summaries.len(), 1, "{captured}");
    assert!(
        summaries[0].contains("quarantined_channels=3"),
        "{captured}"
    );
    let errors: Vec<_> = captured
        .lines()
        .filter(|line| line.contains("pending queue recovery quarantined channel"))
        .collect();
    assert_eq!(errors.len(), 3, "{captured}");
    for (channel, body) in cases {
        let field = format!("channel_id={channel}");
        let matching: Vec<_> = errors.iter().filter(|line| line.contains(&field)).collect();
        assert_eq!(matching.len(), 1, "{captured}");
        let (_, error) = matching[0]
            .split_once("error=")
            .expect("channel error must be reported");
        assert!(!error.trim().is_empty(), "{captured}");
        assert_eq!(
            std::fs::read(dir.join(format!("{channel}.json"))).unwrap(),
            body.as_bytes()
        );
    }
}

/// Adopts the channel's own V2 queue file with a scratch owner, as a live turn would.
#[cfg(unix)]
async fn owner_file(
    shared: &SharedData,
    channel: u64,
) -> (
    crate::services::turn_orchestrator::ChannelMailboxHandle,
    std::path::PathBuf,
) {
    let channel = ChannelId::new(channel);
    let context = super::super::queue_dispatch::persistence_context;
    let persistence = context(shared, &shared.provider, channel);
    let queue = crate::services::turn_orchestrator::v2_queue(&persistence, channel);
    let (path, namespace) = queue.unwrap();
    let scratch = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
    let handle = scratch.handle(channel);
    handle.v2_adopt(path.clone(), namespace).await.unwrap();
    (handle, path)
}

#[cfg(unix)]
fn queued(channel: u64, n: u64) -> crate::services::turn_orchestrator::PendingQueueItem {
    let id = 1_490_000_000_000_008_000_u64 + channel % 1_000 * 10 + n;
    serde_json::from_value(
        serde_json::json!({"author_id":7,"message_id":id,"channel_id":channel,
        "source_message_ids":[id],"queued_generation":5,"text":format!("input {n}"),
        "reply_context":null}),
    )
    .unwrap()
}

/// Queues `item` without a ticket, reserves and arms it; `Some(at)` settles it as submitted at
/// `at`, `None` lets its confirmation window lapse so the original is retained.
#[cfg(unix)]
async fn attempted(
    handle: &crate::services::turn_orchestrator::ChannelMailboxHandle,
    item: Option<crate::services::turn_orchestrator::PendingQueueItem>,
    settled_at: Option<Option<u64>>,
) {
    let attempt = serde_json::json!({"nonce":"boot","provider":"claude",
        "target":{"host":"tmux","owner":"boot","root":"/test","execution_nonce":"e"},
        "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,
            "binding_watermark":null}});
    handle
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            if let Some(item) = item {
                owner.enqueue_fixture(&item).map_err(|e| format!("{e:?}"))?;
            }
            let Some(settled_at) = settled_at else {
                return Ok(());
            };
            let attempt = serde_json::from_value(attempt).map_err(|e| e.to_string())?;
            let key =
                owner.reserve_unit(attempt, |item| Some((String::new(), item.text.clone())))?;
            drop(owner.arm(&key, 1)?);
            match settled_at {
                Some(at) => owner.settle(&key, true, at),
                None => owner.tick(40_000),
            }
        })
        .await
        .unwrap();
}

/// A restart re-adopts only V2 queue files that still hold an input; an input-free file mints no
/// mailbox for health to judge and stays on disk for the next input's owner with its history.
#[cfg(unix)]
#[test]
fn boot_readopts_only_v2_queue_files_that_still_hold_an_input() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (shared, previous) = {
        let _entered = runtime.enter();
        let make = crate::services::discord::make_shared_data_for_tests;
        (make(), make())
    };
    let provider = shared.provider.clone();
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let (fresh, drained, cleared) = (6_813_301_u64, 6_813_302_u64, 6_813_306_u64);
    let (pending, retained, settled) = (6_813_303_u64, 6_813_304_u64, 6_813_305_u64);
    let files = runtime.block_on(async {
        let mut files = Vec::new();
        // Adopted for an input that never came: the file a first routine adoption writes.
        files.push(owner_file(&shared, fresh).await.1);
        // Admitted by ticket, delivered and aged out: only the receipt and sequence remain.
        let (handle, path) = owner_file(&shared, drained).await;
        let ticket = "drained-ticket".to_string();
        handle
            .v2_admit_ticket(queued(drained, 1), ticket)
            .await
            .unwrap();
        attempted(&handle, None, Some(Some(2))).await;
        let aged = handle.v2_request(move |slot| slot.as_mut().ok_or("no owner")?.tick(now));
        aged.await.unwrap();
        files.push(path);
        // One input each, queued, retained or settled, with no ticket bound to it.
        let shapes = [
            (pending, None),
            (retained, Some(None)),
            (settled, Some(Some(now))),
        ];
        for (channel, settle) in shapes {
            let (handle, path) = owner_file(&shared, channel).await;
            attempted(&handle, Some(queued(channel, 1)), settle).await;
            files.push(path);
        }
        // Admitted by ticket, held by a modal before any key, then discarded by /clear in the
        // previous run: a queued bind outlives its item.
        let channel = ChannelId::new(cleared);
        let persistence = super::super::queue_dispatch::persistence_context;
        let persistence = persistence(&previous, &provider, channel);
        let handle = previous.mailbox(channel);
        handle
            .v2_ensure_channel(&persistence, channel)
            .await
            .unwrap();
        let ticket = "cleared-ticket".to_string();
        let admitted = handle.v2_admit_ticket(queued(cleared, 1), ticket).await;
        admitted.unwrap();
        let attempt = serde_json::json!({"nonce":"cleared","provider":"claude",
            "target":{"host":"tmux","owner":"cleared","root":"/test","execution_nonce":"e"},
            "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,
                "binding_watermark":null}});
        let payload = serde_json::json!({"frame":"frame","digest":"digest","attribution":{}});
        let reserved = handle.v2_request(move |slot| {
            let attempt = serde_json::from_value(attempt).map_err(|e| e.to_string())?;
            let payload = serde_json::from_value(payload).map_err(|e| e.to_string())?;
            let owner = slot.as_mut().ok_or("no owner")?;
            owner.reserve(attempt, payload).map(drop)
        });
        reserved.await.unwrap();
        use crate::services::discord::queue_io::input_owner::effect;
        let modal: effect::Leaf = Box::new(|_| {
            crate::services::claude_tui::owner_input::vetoed(effect::MODAL);
            Ok(effect::Completion::Held(effect::MODAL.into()))
        });
        let held = effect::run(&handle, Arc::new(|| 1), modal).await;
        assert_eq!(held, Ok(effect::Completion::NotSent(effect::MODAL.into())));
        let state = handle.v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
        let state = state.await.unwrap();
        let kinds: Vec<_> = (state["admission_binds"].as_array().unwrap().iter())
            .map(|bind| bind["at"]["kind"].as_str().unwrap().to_string())
            .collect();
        assert!(state["head_injection"].is_null(), "{state}");
        assert_eq!(state["items"].as_array().map(Vec::len), Some(1), "{state}");
        assert_eq!(
            kinds,
            ["queued"],
            "the hold keeps the queued bind alone: {state}"
        );
        crate::services::discord::mailbox_clear_channel(&previous, &provider, channel).await;
        files.push(
            crate::services::turn_orchestrator::v2_queue(&persistence, channel)
                .unwrap()
                .0,
        );
        files
    });
    let before: Vec<_> = files.iter().map(|f| std::fs::read(f).unwrap()).collect();
    // Each input-holding file keeps its input in one field alone, so none rides on a bind.
    for bytes in &before[2..5] {
        let json: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        assert!(json.get("admission_binds").is_none(), "{json}");
    }
    let drained_file: serde_json::Value = serde_json::from_slice(&before[1]).unwrap();
    assert_eq!(
        drained_file["terminal_identities"],
        serde_json::json!([]),
        "{drained_file}"
    );
    assert!(
        drained_file.get("admission_binds").is_none(),
        "{drained_file}"
    );
    let cleared_file: serde_json::Value = serde_json::from_slice(&before[5]).unwrap();
    for field in ["items", "retained_inputs", "terminal_identities"] {
        assert_eq!(cleared_file[field], serde_json::json!([]), "{cleared_file}");
    }
    assert!(cleared_file["head_injection"].is_null(), "{cleared_file}");
    assert_eq!(cleared_file["admission_binds"][0]["at"]["kind"], "queued");
    assert_eq!(cleared_file["admission_sequence"], 1, "{cleared_file}");
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    runtime.block_on(async {
        restore_queued_and_inflight_work(&http, &shared, &provider).await;
        let mut minted: Vec<u64> = (shared.mailboxes.snapshot_all().await)
            .into_keys()
            .map(|channel| channel.get())
            .collect();
        minted.sort_unstable();
        assert_eq!(
            minted,
            [pending, retained, settled],
            "only a file that holds an input gets a mailbox at boot"
        );
        for channel in [pending, retained, settled] {
            let owned = shared.mailbox_peek(ChannelId::new(channel)).unwrap();
            let owned = owned.v2_request(|slot| Ok(slot.is_some())).await;
            assert_eq!(owned, Ok(true), "channel {channel} is re-adopted");
        }
        for skipped in [0, 1, 5] {
            assert_eq!(std::fs::read(&files[skipped]).unwrap(), before[skipped]);
        }
        // The next input adopts the kept file: its admission history carries over.
        let channel = ChannelId::new(drained);
        let context = super::super::queue_dispatch::persistence_context;
        let persistence = context(&shared, &provider, channel);
        let handle = shared.mailbox(channel);
        handle
            .v2_ensure_channel(&persistence, channel)
            .await
            .unwrap();
        let state = handle.v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
        let state = state.await.unwrap();
        assert_eq!(state["admission_sequence"], 1, "{state}");
        let receipts = state["admission_receipts"].as_array().map(Vec::len);
        assert_eq!(receipts, Some(1), "{state}");
        // The cleared input's ticket stays spent, and a new ticket queues a new input.
        let channel = ChannelId::new(cleared);
        let persistence = context(&shared, &provider, channel);
        let handle = shared.mailbox(channel);
        handle
            .v2_ensure_channel(&persistence, channel)
            .await
            .unwrap();
        let again = "cleared-ticket".to_string();
        handle
            .v2_admit_ticket(queued(cleared, 1), again)
            .await
            .unwrap();
        let state = handle.v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
        let state = state.await.unwrap();
        assert_eq!(state["items"], serde_json::json!([]), "{state}");
        assert_eq!(state["admission_sequence"], 1, "{state}");
        let next = "cleared-next".to_string();
        handle
            .v2_admit_ticket(queued(cleared, 2), next)
            .await
            .unwrap();
        let state = handle.v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
        let state = state.await.unwrap();
        assert_eq!(state["items"].as_array().map(Vec::len), Some(1), "{state}");
        assert_eq!(state["admission_sequence"], 2, "{state}");
    });
}
