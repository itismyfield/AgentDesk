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
