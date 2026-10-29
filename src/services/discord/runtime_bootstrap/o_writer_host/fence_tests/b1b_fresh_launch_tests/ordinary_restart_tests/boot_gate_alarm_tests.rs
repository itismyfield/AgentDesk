//! Boot alarm timing and terminal reports use the real persisted canary metadata.

use super::*;
use crate::services::discord::commands::control::codex_clear_relaunch_tests::CanaryFixture;
use crate::services::tui_o::alarm::{
    AlarmHealth, AlarmNotification, AlarmNotifier, NotificationAttempt,
};
use crate::services::tui_o::writer::{AlarmSink, WriterAlarm};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use tokio::sync::watch;

#[derive(Default)]
struct Notices(Mutex<Vec<NotificationAttempt>>);

impl AlarmNotifier for Notices {
    fn notify(&self, _: u64, _: AlarmNotification, attempt: NotificationAttempt) {
        self.0.lock().unwrap().push(attempt);
    }
}

struct Reports {
    router: AlarmRouter,
    notices: Arc<Notices>,
    health: Arc<AlarmHealth>,
    halted: Mutex<Vec<String>>,
    pending: AtomicBool,
    cleared: AtomicUsize,
}

impl Reports {
    fn new() -> Self {
        let notices = Arc::new(Notices::default());
        let health = Arc::new(AlarmHealth::default());
        Self {
            router: AlarmRouter::new(
                Some(CANARY_CHANNEL + 9900),
                Some(notices.clone()),
                health.clone(),
            ),
            notices,
            health,
            halted: Mutex::default(),
            pending: AtomicBool::new(false),
            cleared: AtomicUsize::new(0),
        }
    }

    fn details(&self) -> Vec<String> {
        self.halted.lock().unwrap().clone()
    }
}

impl AlarmSink for Reports {
    fn raise(&self, channel: u64, alarm: WriterAlarm) {
        if let WriterAlarm::Halted { detail } = &alarm {
            self.halted.lock().unwrap().push(detail.clone());
        }
        self.router.raise(channel, alarm);
    }

    fn resume_pending(&self, channel: u64, pending: bool) {
        self.pending.store(pending, Ordering::SeqCst);
        self.router.resume_pending(channel, pending);
    }

    fn halt_cleared(&self, channel: u64) {
        self.cleared.fetch_add(1, Ordering::SeqCst);
        self.router.halt_cleared(channel);
    }
}

#[derive(Clone, Copy)]
enum Ending {
    Grace,
    Terminal,
    Stop,
}

async fn boot_wait(ending: Ending) {
    let fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let _selected = test_override::force_channels(&[(CANARY_CHANNEL, CodexTui)]);
    let bytes = fs::read(&fixture.old_source.path).unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, fixture.root.path())
        .unwrap()
        .unwrap();
    let era = store
        .begin_era(&[CANARY_CHANNEL], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: vec![InitSource {
                    source_id: fixture.old_source.clone(),
                    delivery_start: bytes.len() as u64,
                    prefix_hash: hex::encode(Sha256::digest(&bytes)),
                }],
                initial_anchor: 0,
                build_digest: "boot-gate-alarm-test".into(),
                at: chrono::Utc::now(),
            })
        })
        .unwrap();
    let mut channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
    let proof = dedupe::binding_events::codex::read_ownership(&fixture.context)
        .unwrap()
        .verified
        .unwrap();
    channel.set_binding_checkpoint(proof.seq).unwrap();
    channel
        .append_ledger(LedgerEntry::Prepared {
            notice_origin: None,
            serial: channel.ledger().next_serial(),
            unit_key: UnitKey {
                channel_id: CANARY_CHANNEL,
                provider: ShadowProvider::Codex,
                native_key: "waiting-boot-unit".into(),
                kind: UnitKind::Body,
            },
            piece_index: 0,
            payload: "unresolved work must remain held".into(),
            anchor_id: channel.ledger().anchor(),
            epoch: 1,
            local_retry: None,
        })
        .unwrap();
    assert!(dedupe::clear_relaunch::fresh_scope_eligible(CANARY_CHANNEL));
    fresh_process_tests::remove_binding(fresh_process_tests::BindingBoot::Missing);
    assert_eq!(
        dedupe::codex_verified_hold_reason(CANARY_TMUX),
        "binding_missing"
    );
    let reports = Reports::new();
    let (stop, stopping) = watch::channel(false);
    tokio::time::pause();
    let result = std::panic::AssertUnwindSafe(async {
        let waiting = host::test_io::wait_metadata(&channel, &reports, &stopping);
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        if matches!(ending, Ending::Grace) {
            tokio::time::advance(Duration::from_secs(2)).await;
            assert!(futures::poll!(&mut waiting).is_pending());
            assert!(
                reports.details().is_empty(),
                "the grace must not consume a halt notification slot"
            );
            assert!(reports.pending.load(Ordering::SeqCst));
            assert!(
                reports
                    .health
                    .current_at(std::time::Instant::now())
                    .contains(&format!("tui_o:resume_pending:{CANARY_CHANNEL}"))
            );
            assert!(reports.notices.0.lock().unwrap().is_empty());
        }
        tokio::time::advance(Duration::from_secs(11)).await;
        assert!(futures::poll!(&mut waiting).is_pending());
        assert_eq!(reports.details().len(), 1);
        assert_eq!(reports.notices.0.lock().unwrap().len(), 1);
        if matches!(ending, Ending::Terminal) {
            let selection = fixture
                .root
                .path()
                .join("o_store")
                .join(CANARY_CHANNEL.to_string())
                .join("fresh_selection");
            fs::write(selection, b"invalid persisted selection").unwrap();
            tokio::time::advance(Duration::from_secs(30)).await;
            assert!(!waiting.await);
            assert_eq!(
                reports.details().len(),
                2,
                "a final refusal must replace the temporary gate detail"
            );
            assert!(reports.details()[1].ends_with("store_selection_unready"));
            assert_eq!(
                reports.notices.0.lock().unwrap().len(),
                1,
                "the real router deduplicates the halt notification"
            );
            assert!(!reports.pending.load(Ordering::SeqCst));
        } else {
            if matches!(ending, Ending::Grace) {
                assert!(reports.details()[0].ends_with("channel_gate_unready:binding_missing"));
            }
            stop.send(true).unwrap();
            assert!(!waiting.await);
            assert_eq!(
                reports.cleared.load(Ordering::SeqCst),
                1,
                "stopping a warned wait must clear halted health"
            );
            assert!(!reports.pending.load(Ordering::SeqCst));
            assert!(
                !reports
                    .health
                    .current_at(std::time::Instant::now())
                    .contains(&format!("tui_o:halted:{CANARY_CHANNEL}"))
            );
        }
        assert_eq!(fs::read(&fixture.old_source.path).unwrap(), bytes);
        assert!(channel.ledger().unresolved().is_some());
    })
    .catch_unwind()
    .await;
    tokio::time::resume();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn boot_gate_missing_binding_notifies_only_after_grace_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::boot_gate_missing_binding_notifies_only_after_grace_pg"
    )) {
        boot_wait(Ending::Grace).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn boot_gate_terminal_refusal_replaces_warned_detail_once_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::boot_gate_terminal_refusal_replaces_warned_detail_once_pg"
    )) {
        boot_wait(Ending::Terminal).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn boot_gate_stop_clears_a_warned_halt_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::boot_gate_stop_clears_a_warned_halt_pg"
    )) {
        boot_wait(Ending::Stop).await;
    }
}
