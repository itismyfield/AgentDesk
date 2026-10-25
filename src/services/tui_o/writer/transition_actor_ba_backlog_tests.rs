//! Scale/restore coverage keeps the controlled host observer but uses the actual reader/tend,
//! native cursor/P5 store and alarm router; it does not claim production host discovery coverage.
use super::*;
use crate::services::tui_o::alarm::{AlarmHealth, AlarmRouter};
use crate::services::tui_o::writer::rotation::MAX_READERS;

const OLD_NATIVE: usize = 20;

#[derive(Clone)]
struct BacklogHealth {
    observed: Alarms,
    router: Arc<AlarmRouter>,
}

impl AlarmSink for BacklogHealth {
    fn raise(&self, channel: u64, alarm: WriterAlarm) {
        self.observed.raise(channel, alarm.clone());
        self.router.raise(channel, alarm);
    }
    fn reconcile_reader_count(&self, channel: u64, count: usize) {
        self.observed.reconcile_reader_count(channel, count);
        self.router.reconcile_reader_count(channel, count);
    }
    fn waiting_cleared(&self, channel: u64) {
        self.observed.waiting_cleared(channel);
        self.router.waiting_cleared(channel);
    }
    fn reconcile_boundary_pending(&self, channel: u64, count: usize) {
        self.router.reconcile_boundary_pending(channel, count);
    }
    fn resume_pending(&self, channel: u64, pending: bool) {
        self.router.resume_pending(channel, pending);
    }
    fn halt_cleared(&self, channel: u64) {
        self.router.halt_cleared(channel);
    }
}

fn healthy_writer(
    chain: &Chain,
    router: &Arc<AlarmRouter>,
) -> ChannelWriter<FakePort, Arc<FakeLease>, BacklogHealth> {
    let h = &chain.harness;
    ChannelWriter::new(
        h.channel(),
        h.gate.clone(),
        h.port.clone(),
        h.lease.clone(),
        BacklogHealth {
            observed: h.alarms.clone(),
            router: router.clone(),
        },
    )
}

fn degraded(health: &AlarmHealth) -> bool {
    health
        .current_at(std::time::Instant::now())
        .contains(&format!("tui_o:too_many_readers:{CHANNEL}"))
}

async fn targeted_old(
    chain: &Chain,
    run: &Run,
    observer: &HostObservation,
    n: usize,
) -> (Request, ScopeDigest) {
    let mut request = chain.request(&format!("ba-backlog-{n}"));
    request.termination = None;
    let discovery = ask(
        run,
        Operation::Inspect(request.clone()),
        context(chain, observer),
    )
    .await;
    let observation = discovery
        .diagnostics
        .as_ref()
        .unwrap()
        .codex
        .as_ref()
        .unwrap();
    let CodexObservation::Complete(snapshot) = observation else {
        panic!("complete fixture: {observation:?}");
    };
    let claim = for_hop(
        snapshot,
        &chain.key(n),
        &chain.sources[n],
        n as u64 + 2,
        &chain.pane,
    )
    .unwrap();
    request.termination = Some(Termination::Retention(Box::new(claim)));
    let inspected = ask(
        run,
        Operation::Inspect(request.clone()),
        context(chain, observer),
    )
    .await;
    let hop = inspected
        .hops
        .iter()
        .find(|hop| hop.judged.key == chain.key(n))
        .unwrap();
    assert!(
        matches!(hop.judged.verdict, Verdict::Eligible(_)),
        "target {n}: {hop:?}"
    );
    assert!(!chain.cursor(n).retired, "Eligible is not Retired");
    (request, inspected.scope.unwrap().unwrap().digest)
}

#[tokio::test(start_paused = true)]
async fn ba_twenty_native_old_sources_clear_reader_health_only_at_three_and_survive_two_restores() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _hosts = crate::config::session_hosts::force_for_test(Some("mac-mini"), &[]);
    let root = crate::config::runtime_root().unwrap();
    p5::set_test_root(Some(&root));
    let _clear = ClearP5;
    // Source 0 is pre-existing unproven non-native data, followed by 20 aged native sources
    // and the held current native source: 22 real reading readers before retention.
    let chain = native_chain_with_old(OLD_NATIVE);
    let path = root.join(format!("binding_events/{CHANNEL}.log"));
    write_p5(&chain, &path);
    let observer = HostObservation::new(&chain, false, path.clone());
    let health = Arc::new(AlarmHealth::default());
    let router = Arc::new(AlarmRouter::new(None, None, health.clone()));
    let run = run_writer(
        &chain,
        None,
        Arc::default(),
        healthy_writer(&chain, &router),
        ShadowProvider::Codex,
    );
    settle(&chain).await;
    polls(1).await;
    assert_eq!(chain.harness.alarms.readers(), Some(OLD_NATIVE + 2));
    assert!(degraded(&health), "actual actor tend raised reader health");
    assert!((0..chain.sources.len()).all(|n| !chain.cursor(n).retired));
    let p5_before = std::fs::read(&path).unwrap();
    let native_before: Vec<_> = chain
        .paths
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    for n in 1..=OLD_NATIVE {
        let (request, digest) = targeted_old(&chain, &run, &observer, n).await;
        let before = OLD_NATIVE + 3 - n;
        assert_eq!(
            chain.harness.alarms.readers(),
            Some(before),
            "Inspect did not retire target {n}"
        );
        assert_eq!(
            degraded(&health),
            before > MAX_READERS,
            "Inspect alone cannot clear health"
        );
        let reply = ask(
            &run,
            Operation::AutomaticApply(
                request,
                digest,
                Idle(Arc::new(|| Box::pin(std::future::ready(true)))),
            ),
            context(&chain, &observer),
        )
        .await;
        let hop = reply
            .hops
            .iter()
            .find(|hop| hop.judged.key == chain.key(n))
            .unwrap();
        assert!(
            matches!(hop.outcome, Outcome::Retired(_)),
            "target {n}: {hop:?}"
        );
        assert!(chain.cursor(n).retired);
        assert!(
            chain
                .cursor(n)
                .transition_cleanup
                .unwrap()
                .retention
                .is_some()
        );
        polls(1).await;
        let expected = OLD_NATIVE + 2 - n;
        assert_eq!(
            chain.harness.alarms.readers(),
            Some(expected),
            "actual tend count after target {n}"
        );
        assert_eq!(
            degraded(&health),
            expected > MAX_READERS,
            "actual router health after target {n}"
        );
        assert!(
            !chain.cursor(0).retired,
            "unproven backlog is never inferred empty"
        );
        assert!(
            !chain.cursor(OLD_NATIVE + 1).retired,
            "current source remains held"
        );
    }
    assert_eq!(chain.harness.alarms.readers(), Some(2));
    let cursors_before: Vec<_> = (0..chain.sources.len())
        .map(|n| chain.cursor_file(n))
        .collect();
    let posts_before = chain.harness.port.posts().len();
    halt(run.stop, run.task).await;
    for cycle in 1..=2 {
        *chain.harness.alarms.readers.lock().unwrap() = None;
        let restored = run_writer(
            &chain,
            None,
            Arc::default(),
            healthy_writer(&chain, &router),
            ShadowProvider::Codex,
        );
        polls(3).await;
        assert!(
            !restored.task.is_finished(),
            "restore {cycle}: actor remains running"
        );
        assert_eq!(
            chain.harness.alarms.readers(),
            Some(2),
            "restore {cycle}: reader count remains constant"
        );
        assert!(!degraded(&health), "restore {cycle}: no stale reader alarm");
        assert_eq!(
            chain.harness.port.posts().len(),
            posts_before,
            "restore does not replay delivered output"
        );
        for (n, before) in cursors_before.iter().enumerate() {
            assert_eq!(
                &chain.cursor_file(n),
                before,
                "restore {cycle}: exact cursor/audit bytes {n}"
            );
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            p5_before,
            "restore {cycle}: P5 unchanged"
        );
        for (path, before) in chain.paths.iter().zip(&native_before) {
            assert_eq!(
                &std::fs::read(path).unwrap(),
                before,
                "native source unchanged"
            );
        }
        halt(restored.stop, restored.task).await;
    }
}
