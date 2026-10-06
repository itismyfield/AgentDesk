//! The clear entry reading the rotation projection a real hosted O actor published to this
//! process's readiness, with no forced count.

use crate::services::claude_tui::hook_server::HookEventKind;
use crate::services::tui_o::cutover::test_override as cutover;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{self, ShadowProvider, binding_reader::source_id_for};
use crate::services::tui_o::writer::activation::ActivationFacts;
use crate::services::tui_o::writer::actor::POLL_INTERVAL;
use crate::services::tui_o::writer::adoption::{LegacyView, NoLegacy};
use crate::services::tui_o::writer::binding as o;
use crate::services::tui_o::writer::host::{self, Custody, HostIo, HostParts, test_io};

use super::*;

/// The pane's log as O reads it: startup, then each clear's bind onto a new source.
struct Hops {
    events: Mutex<Vec<o::BindingEvent>>,
    notice: tokio::sync::watch::Sender<u64>,
}

impl Hops {
    fn new() -> Arc<Self> {
        let notice = tokio::sync::watch::channel(0).0;
        Arc::new(Self {
            events: Mutex::default(),
            notice,
        })
    }

    fn bind(&self, channel: u64, old: Option<&shadow::SourceId>, new: &shadow::SourceId) {
        let mut events = self.events.lock().unwrap();
        let seq = events.len() as u64 + 1;
        let cause = match old {
            None => o::BindingCause::Startup,
            Some(_) => o::BindingCause::Clear,
        };
        let evidence = o::BindingEvidence {
            hook_event: HookEventKind::SessionStart.as_str().into(),
            received_at: chrono::Utc::now(),
            reclaims: false,
        };
        events.push(o::BindingEvent {
            seq,
            channel_id: channel,
            provider: ShadowProvider::Claude,
            tmux_session: "pane".into(),
            execution_nonce: "nonce".into(),
            record: o::BindingRecord::Bound {
                old: old.cloned(),
                new: o::BindingTarget::Source(new.clone()),
                cause,
                parent_hint: old.cloned(),
                evidence,
            },
            committed_at: chrono::Utc::now(),
        });
        self.notice.send_replace(seq);
    }
}

impl o::BindingEvents for Hops {
    fn binding_events_since(
        &self,
        channel: u64,
        after: u64,
    ) -> Result<Vec<o::BindingEvent>, String> {
        let events = self.events.lock().unwrap();
        let since = events
            .iter()
            .filter(|e| e.channel_id == channel && e.seq > after);
        Ok(since.cloned().collect())
    }

    fn subscribe(&self, _: u64) -> tokio::sync::watch::Receiver<u64> {
        self.notice.subscribe()
    }
}

/// The gateway side of the writer host over that log; nothing of Legacy holds the channel.
struct Gateway {
    posts: Arc<test_io::Posts>,
    alarms: test_io::Alarms,
    log: Arc<Hops>,
}

impl HostIo for Gateway {
    type Port = test_io::Posts;
    type Lease = test_io::AnyLease;
    type Alarms = test_io::Alarms;
    type Bindings = Hops;

    fn port(&self) -> impl Future<Output = Arc<test_io::Posts>> + Send {
        std::future::ready(Arc::clone(&self.posts))
    }

    fn lease(&self) -> test_io::AnyLease {
        test_io::AnyLease
    }

    fn alarms(&self) -> test_io::Alarms {
        self.alarms.clone()
    }

    fn bindings(&self, _: u64, _: ShadowProvider) -> Arc<Hops> {
        Arc::clone(&self.log)
    }

    fn activation_facts(
        &self,
        _: u64,
        _: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
        std::future::ready(Ok(ActivationFacts::default()))
    }

    fn local_custody(&self, _: u64, _: ShadowProvider) -> Result<Custody, String> {
        Ok(Custody::Free)
    }

    fn legacy(&self) -> Arc<dyn LegacyView> {
        Arc::new(NoLegacy)
    }

    fn legacy_busy(&self, _: u64) -> impl Future<Output = bool> + Send {
        std::future::ready(false)
    }

    fn relaying(&self, _: u64) -> bool {
        false
    }
}

/// Drops the fixture's forced projection, so the entry reads this process's readiness.
fn unforce_projection(fx: &mut Fixture) {
    let probe = host::force_unsettled_for_test(None);
    let forced = std::any::Any::type_id(&probe);
    drop(probe);
    let at = fx
        ._thread
        .iter()
        .position(|guard| (**guard).type_id() == forced);
    drop(
        fx._thread
            .remove(at.expect("the fixture forces a projection")),
    );
}

// No actor gives no count, a clear's rotated-away source the running actor has not retired gives
// a typed refusal before any change, and once O retires it the entry clears with one line.
#[test]
fn the_clear_entry_follows_a_hosted_actors_projection_until_its_rotation_settles_pg() {
    let mut fx = Fixture::new(14);
    unforce_projection(&mut fx);
    let channel = fx.channel_id.get();
    let refused = |what: &str| {
        let error = fx.rt.block_on(fx.clear()).unwrap_err().to_string();
        fx.rt.block_on(fx.assert_untouched(what));
        error
    };
    let error = refused("no actor");
    assert!(error.contains("rotation_unread"), "{error}");

    let o_root = tempfile::tempdir().unwrap();
    let a_path = o_root.path().join("a.jsonl");
    std::fs::write(&a_path, b"").unwrap();
    let a = source_id_for("s1", &a_path).unwrap();
    let log = Hops::new();
    log.bind(channel, None, &a);
    let io = Arc::new(Gateway {
        posts: Arc::default(),
        alarms: test_io::Alarms::default(),
        log: Arc::clone(&log),
    });
    let _candidates = cutover::force_candidates(&[(channel, ClaudeTui)]);
    let gate = Arc::new(OwnershipGate::default());
    let writer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let polls =
        |count: u32| writer.block_on(async { tokio::time::sleep(POLL_INTERVAL * count).await });
    let parts = || HostParts {
        io: Arc::clone(&io),
        runtime_root: Some(o_root.path().to_path_buf()),
        gate: Arc::clone(&gate),
        readiness: host::process_readiness(),
    };
    let tasks = writer.block_on(async { host::start(ShadowProvider::Claude, true, parts) });
    gate.acquired();
    polls(3);
    let alarms = || io.alarms.0.lock().unwrap().clone();
    assert_eq!(host::rotation_unsettled(channel), Some(0), "{:?}", alarms());

    let b_path = o_root.path().join("b.jsonl");
    let row = json!({"type": "assistant", "uuid": "u-n1", "apiBlockIndex": 0,
        "message": {"id": "n1", "content": [{"type": "text", "text": "new first"}]}});
    std::fs::write(&b_path, format!("{row}\n")).unwrap();
    log.bind(channel, Some(&a), &source_id_for("s2", &b_path).unwrap());
    polls(3);
    assert_eq!(host::rotation_unsettled(channel), Some(1), "{:?}", alarms());
    let error = refused("unsettled");
    assert!(error.contains("rotation_unsettled(1)"), "{error}");
    assert_eq!(fx.rt.block_on(fx.session()), (Some("old".into()), false));

    polls(12);
    assert_eq!(
        host::rotation_unsettled(channel),
        Some(0),
        "retired once quiet"
    );
    fx.rt.block_on(async {
        let result = fx
            .clear_with(|| fx.record("new", BindingCause::Clear, true))
            .await;
        assert_eq!(fx.rig.sends(), clear_line(), "one gated line");
        assert_eq!(fx.session().await, (Some("new".into()), true));
        result.expect("the settled rotation admits the clear");
    });
    assert_eq!(io.posts.to(channel), ["new first"]);
    tasks.iter().for_each(tokio::task::JoinHandle::abort);
    polls(1);
}
