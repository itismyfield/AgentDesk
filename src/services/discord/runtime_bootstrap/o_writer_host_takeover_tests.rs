//! The Codex takeover through the gateway's own facts, intake fence and custody: the fence is held
//! until the init is written, and a Legacy body claimed between custody's two reads is O's.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use super::*;
use crate::config::Config;
use crate::services::agent_protocol::RuntimeHandoffKind::CodexTui;
use crate::services::discord::tui_direct_pending_start::turn_retirement::codex::confirm_codex_channels;
use crate::services::tui_o::channel_policy::BootChannels;
use crate::services::tui_o::cutover::{o_owns_tui_output_for_channel_reserving, test_override};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::turn_mode::{TurnConfig, transcript_turns};
use crate::services::tui_o::writer::activation::ActivationFacts;
use crate::services::tui_o::writer::activation::test_hook::{self, Step};
use crate::services::tui_o::writer::adoption::LegacyView;
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingEvents, BindingEvidence, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::late_adopt;

/// Binds as the hook writer would; reads are counted so a bind can land after the baseline.
struct Log {
    channel: u64,
    events: Mutex<Vec<BindingEvent>>,
    notice: watch::Sender<u64>,
    reads: AtomicUsize,
}

impl Log {
    fn new(channel: u64) -> Arc<Self> {
        Arc::new(Self {
            channel,
            events: Mutex::default(),
            notice: watch::channel(0).0,
            reads: AtomicUsize::new(0),
        })
    }

    /// Binds `source` once the log was read more than `reads` times.
    async fn bind_after(&self, reads: usize, source: &SourceId) {
        while self.reads.load(Ordering::SeqCst) <= reads {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut events = self.events.lock().unwrap();
        let seq = events.len() as u64 + 1;
        let evidence = BindingEvidence {
            hook_event: "session_start".into(),
            received_at: chrono::Utc::now(),
            reclaims: false,
        };
        events.push(BindingEvent {
            seq,
            channel_id: self.channel,
            provider: Codex,
            tmux_session: "takeover".into(),
            execution_nonce: "takeover".into(),
            record: BindingRecord::Bound {
                old: None,
                new: BindingTarget::Source(source.clone()),
                cause: BindingCause::Clear,
                parent_hint: None,
                evidence,
            },
            committed_at: chrono::Utc::now(),
        });
        drop(events);
        self.notice.send_replace(seq);
    }
}

impl BindingEvents for Log {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        assert_eq!(channel, self.channel);
        self.reads.fetch_add(1, Ordering::SeqCst);
        let events = self.events.lock().unwrap();
        Ok(events.iter().filter(|e| e.seq > after).cloned().collect())
    }

    fn subscribe(&self, _: u64) -> watch::Receiver<u64> {
        self.notice.subscribe()
    }
}

/// The gateway host with late adoption on and the test's binding log; all else is the gateway's.
struct Late<H> {
    host: H,
    log: Arc<Log>,
}

impl<H: HostIo> HostIo for Late<H> {
    type Port = H::Port;
    type Lease = H::Lease;
    type Alarms = H::Alarms;
    type Bindings = Log;
    fn port(&self) -> impl Future<Output = Arc<H::Port>> + Send {
        self.host.port()
    }
    fn lease(&self) -> H::Lease {
        self.host.lease()
    }
    fn alarms(&self) -> H::Alarms {
        self.host.alarms()
    }
    fn bindings(&self, _: u64, _: ShadowProvider) -> Arc<Log> {
        Arc::clone(&self.log)
    }
    fn late_adopt_enabled(&self) -> bool {
        true
    }
    fn activation_facts(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
        self.host.activation_facts(channel, provider)
    }
    fn intake_fence(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<FencedFacts, String>> + Send {
        self.host.intake_fence(channel, provider)
    }
    fn local_custody(&self, channel: u64, provider: ShadowProvider) -> Result<Custody, String> {
        self.host.local_custody(channel, provider)
    }
    fn legacy(&self) -> Arc<dyn LegacyView> {
        self.host.legacy()
    }
    fn legacy_busy(&self, channel: u64) -> impl Future<Output = bool> + Send {
        self.host.legacy_busy(channel)
    }
    fn relaying(&self, channel: u64) -> bool {
        self.host.relaying(channel)
    }
}

/// Each of `channels` reserved for late adoption as the boot seeds it on the home.
fn reserve(channels: &[u64]) -> test_override::ChannelsGuard {
    let agents: Vec<_> = (channels.iter())
        .map(|id| {
            serde_json::json!({"id": format!("w{id}"), "name": "W",
            "channels": {"codex": {"id": id.to_string(), "runtime": "tui"}}})
        })
        .collect();
    let config: Config = serde_json::from_value(serde_json::json!({
        "server": {}, "agents": agents,
        "tui_o": {"writer": {"channels": channels, "late_adopt": true}}
    }))
    .unwrap();
    let states: BTreeMap<_, _> = channels.iter().map(|&c| (c, Adoption::Pending)).collect();
    let boot = BootChannels::validate(&config).unwrap();
    test_override::force_boot(boot.seeded(&config, |_, _| Ok(states)).unwrap())
}

fn candidate(channel: u64) -> Candidate {
    let candidate = test_override::with_channels(|boot| boot?.candidate(channel).cloned());
    candidate.unwrap()
}

/// A rollout after `/clear`: its header, the turn opener and the prompt Legacy took in.
fn rollout(dir: &std::path::Path, name: &str) -> SourceId {
    let path = dir.join(name);
    let lines = [
        json!({"type":"session_meta","payload":{"id":"takeover","cwd":"/tmp","source":"cli"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user",
            "content":[{"type":"input_text","text":"hi"}]}}),
    ];
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(&path, text).unwrap();
    source_id_for("takeover", &path).unwrap()
}

/// From its own runtime and connection, an intake insert that has not finished after 300ms.
fn insert_waits(url: String) -> (bool, std::thread::JoinHandle<()>) {
    let (waited, reported) = std::sync::mpsc::channel();
    let insert = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let pool = crate::db::postgres::connect_test_pool_with_max_connections(
                &url,
                "takeover fence probe",
                1,
            )
            .await
            .unwrap();
            let insert = open_intake(&pool, 64_700_599, "codex");
            tokio::pin!(insert);
            let timed = tokio::time::timeout(Duration::from_millis(300), &mut insert).await;
            waited.send(timed.is_err()).unwrap();
            if timed.is_err() {
                insert.await;
            }
            pool.close().await;
        });
    });
    let waited = reported.recv_timeout(Duration::from_secs(30)).unwrap();
    (waited, insert)
}

fn init_sources(store: &OStore, channel: u64) -> Vec<(SourceId, u64)> {
    let init = store.read_init(channel).unwrap().unwrap();
    (init.sources.into_iter())
        .map(|source| (source.source_id, source.delivery_start))
        .collect()
}

/// Over Legacy's open intake and Codex pending start an intake insert waits on the fence through
/// the write and turn mode stays off; a body claimed between custody's reads waits, then is O's.
#[test]
fn codex_takeover_holds_the_gateway_fence_through_its_write_and_its_adoption_lock_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(gateway_takeover());
}

async fn gateway_takeover() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let local = crate::services::cluster::node_registry::SELF_INSTANCE_ID.get();
    let local = local.map_or("takeover-local", String::as_str);
    let log = Log::new(64_700_501);
    let io = Late {
        host: test_host::over(shared, Duration::ZERO, Some(local)),
        log: Arc::clone(&log),
    };
    let stores = tempfile::tempdir().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, stores.path());
    let store = store.unwrap().unwrap();
    let rollouts = tempfile::tempdir().unwrap();
    let (fenced_channel, raced) = (64_700_501, 64_700_502);
    let _reserved = reserve(&[fenced_channel, raced]);
    let gate = OwnershipGate::default();
    gate.acquired();

    open_intake(&pool, fenced_channel, "codex").await;
    let record = pending(Codex, fenced_channel);
    persist(&record).unwrap();
    let taken = rollout(rollouts.path(), "rollout-taken.jsonl");
    let probe = Arc::new(Mutex::new(None));
    let (slot, url) = (Arc::clone(&probe), fixture.database_url.clone());
    test_hook::set(fenced_channel, Step::BeforeWrite, move || {
        *slot.lock().unwrap() = Some(insert_waits(url));
        Ok(())
    });
    let adopting = candidate(fenced_channel);
    let activated =
        late_adopt::activate(&io, fenced_channel, Codex, &store, &*log, &adopting, &gate);
    let (committed, ()) = tokio::join!(activated, log.bind_after(0, &taken));
    assert!(committed);
    assert_eq!(adopting.peek(), Adoption::Committed);
    assert_eq!(init_sources(&store, fenced_channel), [(taken, 0)]);
    let (waited, insert) = probe
        .lock()
        .unwrap()
        .take()
        .expect("the takeover wrote its init");
    assert!(
        waited,
        "an intake insert waits on the fence until the init is written"
    );
    insert.join().unwrap();
    let custody = io.local_custody(fenced_channel, Codex);
    assert_eq!(custody, Ok(Custody::PendingStart), "Legacy's start is kept");
    let turn = TurnConfig {
        channels: [fenced_channel].into(),
        all_owned: Some(false),
    };
    assert!(confirm_codex_channels(Some(&turn), || vec![fenced_channel]).is_empty());
    delete(&record);
    // With Legacy's start gone the takeover still waits for its turn in the rollout bound now.
    assert!(confirm_codex_channels(Some(&turn), || vec![fenced_channel]).is_empty());
    assert!(!transcript_turns(fenced_channel));

    let raced_log = Log::new(raced);
    let io = Late {
        host: io.host,
        log: Arc::clone(&raced_log),
    };
    // Legacy's body claim lands between custody's two reads and waits on the adoption lock.
    let claimer = Arc::new(Mutex::new(None));
    let (slot, share) = (Arc::clone(&claimer), test_override::shared_channels());
    test_hook::set(raced, Step::CustodyRead, move || {
        let (at_claim, reached) = std::sync::mpsc::channel();
        let claim = std::thread::spawn(move || {
            let _channels = share();
            at_claim.send(()).unwrap();
            o_owns_tui_output_for_channel_reserving(raced, Some(CodexTui)).map(|c| c.owned)
        });
        reached.recv().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        *slot.lock().unwrap() = Some((!claim.is_finished(), claim));
        Ok(())
    });
    let raced_rollout = rollout(rollouts.path(), "rollout-raced.jsonl");
    let adopting = candidate(raced);
    let activated = late_adopt::activate(&io, raced, Codex, &store, &*raced_log, &adopting, &gate);
    let (committed, ()) = tokio::join!(activated, raced_log.bind_after(0, &raced_rollout));
    assert!(committed);
    assert_eq!(init_sources(&store, raced), [(raced_rollout, 0)]);
    let (waited, claim) = claimer.lock().unwrap().take().expect("custody was read");
    assert!(waited, "the claim waited on the adoption lock");
    assert!(
        matches!(claim.join().unwrap(), Ok(true)),
        "O owns the body: Legacy sends nothing"
    );
    pool.close().await;
    fixture.drop().await;
}
