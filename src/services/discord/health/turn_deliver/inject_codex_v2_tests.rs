#[tokio::test(flavor = "current_thread")]
async fn queued_codex_v2_real_entry_uses_permit_adapter_and_native_receipt_pg() {
    use std::os::unix::fs::MetadataExt;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = QueuedCodex::new(7_272_301).await;
    let mailbox = f.shared.mailbox(ChannelId::new(f.pane.channel));
    mailbox
        .v2_adopt(f.disk_path(), "codex-real-v2".into())
        .await
        .unwrap();
    let transcript = f.pane.dir.path().join("rollout.jsonl");
    let meta = fs::metadata(&transcript).unwrap();
    let attempt = serde_json::json!({"nonce":"7272301a", "provider":"codex",
        "target":{"host":"tmux","owner":f.pane.session(),"root":"/tmp","execution_nonce":"turn-open"},
        "source":{"kind":"existing","session_id":format!("queued-codex-{}",f.pane.channel),
            "transcript":transcript.display().to_string(),"file":[meta.dev(),meta.ino()],"offset":meta.len()}});
    mailbox
        .v2_request(move |slot| {
            slot.as_mut().unwrap().reserve(
                serde_json::from_value(attempt).unwrap(),
                serde_json::from_value(
                    serde_json::json!({"frame":"prepared","digest":"digest","attribution":{}}),
                )
                .unwrap(),
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let result = f.attempt().await;
    assert!(
        matches!(result, InjectAttempt::Injected { .. }),
        "V2 must traverse the actual queued origin, permit, adapter and rollout receipt: {result:?}; disk={}; keys={}",
        f.disk(),
        f.pane.keys()
    );
    let snapshot = mailbox
        .v2_request(|slot| slot.as_ref().unwrap().json())
        .await
        .unwrap();
    assert!(snapshot["items"].as_array().unwrap().is_empty());
    assert_eq!(snapshot["terminal_identities"].as_array().unwrap().len(), 1);
    let keys = f.pane.keys();
    let _ = f.attempt().await;
    assert_eq!(
        f.pane.keys(),
        keys,
        "a settled V2 source must not be typed twice"
    );
}
