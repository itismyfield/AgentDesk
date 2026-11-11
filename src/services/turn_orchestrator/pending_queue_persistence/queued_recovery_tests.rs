use super::*;
use std::io::Write;
use std::sync::{Arc, Mutex};

const TOKEN: &str = "s2b-scan-errors";

fn namespace(provider: &ProviderKind) -> std::path::PathBuf {
    pending_queue_root()
        .unwrap()
        .join(provider.as_str())
        .join(TOKEN)
}

fn seed(dir: &super::super::Path, channel: u64) -> (std::path::PathBuf, Vec<u8>) {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{channel}.json"));
    let bytes = serde_json::to_vec(&serde_json::json!([{
        "author_id": 7,
        "message_id": channel + 100,
        "queued_generation": 7,
        "text": "queued input",
        "channel_id": channel
    }]))
    .unwrap();
    fs::write(&path, &bytes).unwrap();
    (path, bytes)
}

#[derive(Clone)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedLogs {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn permission_denied_is_returned_and_logged_without_changing_the_queue() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let dir = namespace(&provider);
    let (path, bytes) = seed(&dir, 6_813_401);
    let _fault = scan_fault::arm(
        &dir,
        scan_fault::Point::ReadDir,
        io::ErrorKind::PermissionDenied,
    );
    let loaded = load_pending_queues_checked(&provider, TOKEN);
    assert!(loaded.queues.is_empty() && loaded.quarantined.is_empty());
    assert_eq!(loaded.scan_errors.len(), 1);
    assert!(loaded.scan_errors[0].contains("read_dir"));
    assert_eq!(fs::read(&path).unwrap(), bytes);

    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = CapturedLogs(logs.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let _fault = scan_fault::arm(
        &dir,
        scan_fault::Point::ReadDir,
        io::ErrorKind::PermissionDenied,
    );
    tracing::subscriber::with_default(subscriber, || {
        let (queues, overrides) = super::super::load_pending_queues(&provider, TOKEN);
        assert!(queues.is_empty() && overrides.is_empty());
    });
    let captured = String::from_utf8(
        logs.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
    )
    .unwrap();
    let summary: Vec<_> = captured
        .lines()
        .filter(|line| line.contains("pending queue recovery scan errors"))
        .collect();
    assert_eq!(summary.len(), 1, "{captured}");
    assert!(summary[0].contains("scan_errors=1"), "{captured}");
    assert!(
        captured.contains("read_dir") && captured.contains("injected pending queue scan error"),
        "{captured}"
    );
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn an_entry_error_is_returned_while_other_channels_still_restore() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let dir = namespace(&provider);
    let originals = [seed(&dir, 6_813_411), seed(&dir, 6_813_412)];
    let _fault = scan_fault::arm(
        &dir,
        scan_fault::Point::Entry,
        io::ErrorKind::PermissionDenied,
    );
    let loaded = load_pending_queues_checked(&provider, TOKEN);
    assert_eq!(loaded.queues.len(), 1);
    assert!(loaded.quarantined.is_empty());
    assert_eq!(loaded.scan_errors.len(), 1);
    assert!(loaded.scan_errors[0].contains("scan"));
    for (path, bytes) in originals {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn a_missing_namespace_is_empty_without_a_scan_error() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let dir = namespace(&provider);
    assert!(!dir.exists());
    let loaded = load_pending_queues_checked(&provider, TOKEN);
    assert!(loaded.queues.is_empty() && loaded.overrides.is_empty());
    assert!(loaded.quarantined.is_empty() && loaded.scan_errors.is_empty());
    assert!(!dir.exists());
}

#[cfg(unix)]
#[test]
fn settled_history_leaves_cleared_unconfirmed_recovery_idle_without_hiding_inputs() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let dir = namespace(&provider);
    fs::create_dir_all(&dir).unwrap();
    let channel = ChannelId::new(7_962_201);
    let path = dir.join(format!("{}.json", channel.get()));
    let cases: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/e2e/tui_relay/tests/fixtures/queue_v2_cleared_unconfirmed_cases.json"
    )))
    .unwrap();
    let mut base = cases["base"].clone();
    let source = base["admission_binds"][0]["at"]["attempt"]["source"].clone();
    for index in 0..26 {
        let receipt = format!("tk:settled-{index}");
        let settled_at_ms = 1_794_105_480_000_u64 + index * 60_000;
        base["admission_receipts"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!(receipt));
        base["admission_binds"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "receipt": receipt, "at": {"kind": "settled", "attempt": format!("settled-{index}"),
                    "source": source, "settled_at_ms": settled_at_ms}
            }));
        base["terminal_identities"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "identity": {"primary": 200 + index, "queued_generation": 7,
                    "sources": [{"message_id": 200 + index, "queued_generation": 7}]},
                "settled_at_ms": settled_at_ms
            }));
    }
    base["admission_sequence"] = serde_json::json!(27);
    for extra in ["none", "item", "head", "attempt", "opaque", "owed"] {
        let mut snapshot = base.clone();
        match extra {
            "item" => snapshot["items"]
                .as_array_mut()
                .unwrap()
                .push(base["retained_inputs"][0]["input"]["original"].clone()),
            "head" => snapshot["head_injection"] = base["retained_inputs"][0]["input"].clone(),
            "attempt" => {
                let mut bind = base["admission_binds"][0].clone();
                bind["at"]["attempt"]["nonce"] = serde_json::json!("open-attempt");
                snapshot["admission_binds"]
                    .as_array_mut()
                    .unwrap()
                    .push(bind);
            }
            "opaque" => snapshot["admission_binds"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"receipt": "tk:opaque", "at": {"kind": "opaque"}})),
            "owed" => {
                let mut retained = base["retained_inputs"][0].clone();
                retained["notice"]["enqueued"] = serde_json::json!(false);
                snapshot["retained_inputs"]
                    .as_array_mut()
                    .unwrap()
                    .push(retained);
            }
            _ => {}
        }
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        fs::write(&path, &bytes).unwrap();
        let idle = extra == "none";
        let loaded = load_pending_queues_checked(&provider, TOKEN);
        assert_eq!(
            loaded.v2_idle.contains(&channel),
            idle,
            "idle report: {extra}"
        );
        assert_eq!(
            loaded.quarantined.len(),
            usize::from(!idle),
            "quarantine: {extra}"
        );
        assert!(loaded.queues.is_empty() && loaded.scan_errors.is_empty());
        assert_eq!(
            v2_channels(&provider, TOKEN).ready.contains(&channel),
            !idle,
            "boot adoption: {extra}"
        );
        assert!(
            v2_held_sources(&provider, TOKEN, channel)
                .delivered
                .contains(&225)
        );

        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = CapturedLogs(logs.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();
        crate::logging::test_capture::pin_callsite_interest();
        tracing::subscriber::with_default(subscriber, || {
            let (queues, overrides) = super::super::load_pending_queues(&provider, TOKEN);
            assert!(queues.is_empty() && overrides.is_empty());
        });
        let captured =
            String::from_utf8(logs.lock().unwrap_or_else(|p| p.into_inner()).clone()).unwrap();
        assert_eq!(
            captured
                .lines()
                .filter(|line| line.contains("INFO")
                    && line.contains("left a V2 file holding no input"))
                .count(),
            usize::from(idle),
            "{extra}: {captured}"
        );
        assert_eq!(
            captured
                .lines()
                .filter(|line| line.contains("WARN")
                    && line.contains("pending queue recovery quarantined channel"))
                .count(),
            usize::from(!idle),
            "{extra}: {captured}"
        );
        assert_eq!(fs::read(&path).unwrap(), bytes, "file preserved: {extra}");
    }
}

#[cfg(unix)]
#[test]
fn owned_discovery_preserves_named_failures_without_adopting_them() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Codex;
    let dir = namespace(&provider);
    fs::create_dir_all(&dir).unwrap();
    let mut originals = Vec::new();
    for (channel, bytes) in [
        (8_057_121, b"{invalid JSON".to_vec()),
        (8_057_122, vec![0xff, 0xfe]),
        (8_057_123, br#"{"version":99}"#.to_vec()),
    ] {
        let path = dir.join(format!("{channel}.json"));
        fs::write(&path, &bytes).unwrap();
        originals.push((ChannelId::new(channel), path, bytes));
    }
    let unreadable = dir.join("8057124.json");
    fs::create_dir(&unreadable).unwrap();
    seed(&dir, 8_057_125);
    let owned_path = dir.join("8057126.json");
    fs::write(
        &owned_path,
        serde_json::to_vec(&serde_json::json!({
            "version":2, "items": [{"author_id":7,"message_id":8057127,"text":"kept"}],
            "head_injection":null,"retained_inputs":[],"terminal_identities":[]
        }))
        .unwrap(),
    )
    .unwrap();
    let discovery = v2_channels(&provider, TOKEN);
    assert_eq!(discovery.ready, vec![ChannelId::new(8_057_126)]);
    assert!(discovery.namespace_errors.is_empty());
    assert_eq!(discovery.failed.len(), 4);
    for failure in &discovery.failed {
        assert_eq!(failure.provider, "codex");
        assert_eq!(failure.namespace, format!("codex/{TOKEN}"));
        assert_eq!(
            failure.expected_path,
            dir.join(format!("{}.json", failure.channel.get()))
        );
        assert_eq!(failure.stage, "catalog_unreadable");
        assert!(!failure.error.is_empty());
    }
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = CapturedLogs(logs.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let ready = tracing::subscriber::with_default(subscriber, || discovery.observed_ready());
    assert_eq!(ready, vec![ChannelId::new(8_057_126)]);
    let captured = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert_eq!(
        captured
            .lines()
            .filter(|line| line.contains("pending queue recovery quarantined channel"))
            .count(),
        4
    );
    assert!(captured.contains("catalog_unreadable") && captured.contains("codex/s2b-scan-errors"));
    for (_, path, bytes) in originals {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    assert!(unreadable.is_dir());
}

#[cfg(unix)]
#[test]
fn owned_discovery_keeps_namespace_scan_failure_separate_from_named_channels() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Codex;
    let dir = namespace(&provider);
    let (path, bytes) = seed(&dir, 8_057_131);
    let _fault = scan_fault::arm(
        &dir,
        scan_fault::Point::ReadDir,
        io::ErrorKind::PermissionDenied,
    );
    let discovery = v2_channels(&provider, TOKEN);
    assert!(discovery.ready.is_empty() && discovery.failed.is_empty());
    assert_eq!(discovery.namespace_errors.len(), 1);
    assert!(discovery.namespace_errors[0].contains("read_dir"));
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[cfg(unix)]
#[test]
fn outbox_only_v3_is_discovered_without_reclassifying_delivered_sources_as_owed() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Codex;
    let dir = namespace(&provider);
    fs::create_dir_all(&dir).unwrap();
    let channel = ChannelId::new(8_057_141);
    let identity = serde_json::json!({"primary":8057142,"queued_generation":7,
        "sources":[{"message_id":8057142,"queued_generation":7}]});
    let snapshot = serde_json::json!({
        "version":3,"items":[],"head_injection":null,"retained_inputs":[],
        "terminal_identities":[{"identity":identity,"settled_at_ms":1}],
        "namespace":"marker-read","a_control":[{
            "cleanup_id":"cleanup-discovery","identity":identity,
            "front_receipt":"ticket-discovery","epoch":"epoch-discovery",
            "expected":{"kind":"missing","owner":{"provider":"codex",
                "discord_token_hash":TOKEN,"channel_id":channel.get().to_string(),
                "logical_key":"logical-discovery","owner_node":"fixture-node",
                "runtime_root":"/fixture"}},
            "paths":[{"path":"/fixture/canonical.kind","parent":[1,2],
                "entry":{"kind":"absent"}},
                {"path":"/fixture/legacy.kind","parent":[1,4],"entry":{"kind":"absent"}}],
            "stage":{"kind":"outbox","event_id":"stable-discovery-event"},"attempt":null
        }]
    });
    let path = dir.join(format!("{}.json", channel.get()));
    let bytes = serde_json::to_vec_pretty(&snapshot).unwrap();
    fs::write(&path, &bytes).unwrap();
    let found = v2_channels(&provider, TOKEN);
    assert_eq!(found.ready, vec![channel]);
    assert!(found.failed.is_empty() && found.namespace_errors.is_empty());
    let sources = v2_held_sources(&provider, TOKEN, channel);
    assert!(sources.owed.is_empty());
    assert_eq!(sources.delivered, vec![8_057_142]);
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[cfg(unix)]
#[test]
fn control_only_sources_stay_fenced_without_erasing_delivered_or_queued_provenance() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Codex;
    let dir = namespace(&provider);
    fs::create_dir_all(&dir).unwrap();
    let channel = ChannelId::new(8_057_151);
    let identity = |id| {
        serde_json::json!({"primary":id,"queued_generation":7,
        "sources":[{"message_id":id,"queued_generation":7}]})
    };
    let control = |id| {
        serde_json::json!({
            "cleanup_id":format!("cleanup-{id}"),"identity":identity(id),
            "front_receipt":null,"epoch":"epoch-source-fence",
            "expected":{"kind":"missing","owner":{"provider":"codex",
                "discord_token_hash":TOKEN,"channel_id":channel.get().to_string(),
                "logical_key":"logical-source-fence","owner_node":"fixture-node",
                "runtime_root":"/fixture"}},
            "paths":[{"path":"/fixture/canonical.kind","parent":[1,2],
                "entry":{"kind":"absent"}}],
            "stage":{"kind":"outbox","event_id":format!("event-{id}")},"attempt":null
        })
    };
    let mut repeated = control(8057153);
    repeated["cleanup_id"] = serde_json::json!("cleanup-source-repeat");
    repeated["stage"]["event_id"] = serde_json::json!("event-source-repeat");
    let snapshot = serde_json::json!({
        "version":3,"namespace":"marker-source-fence",
        "items":[{"author_id":7,"message_id":8057154,"queued_generation":7,"text":"owed"}],
        "head_injection":null,"retained_inputs":[],
        "terminal_identities":[
            {"identity":identity(8057152),"settled_at_ms":1},
            {"identity":identity(8057154),"settled_at_ms":1}
        ],
        "a_control":[control(8057152),control(8057153),repeated,control(8057154)]
    });
    let path = dir.join(format!("{}.json", channel.get()));
    let bytes = serde_json::to_vec_pretty(&snapshot).unwrap();
    fs::write(&path, &bytes).unwrap();
    let projected = v2_held_sources(&provider, TOKEN, channel);
    assert_eq!(projected.owed, vec![8_057_154, 8_057_153]);
    assert_eq!(projected.delivered, vec![8_057_152]);
    assert_eq!(fs::read(path).unwrap(), bytes);
}
