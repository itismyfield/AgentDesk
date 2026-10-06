//! The production drain port read against a hosted O writer and the channel's mailbox.

use super::*;
use crate::services::tui_o::writer::actor::{Owing, Undelivered};
use crate::services::turn_orchestrator::ChannelMailboxSnapshot;
use crate::services::turn_orchestrator::registry_purge::MailboxRefusal;

// The turn comes from the channel's mailbox: none runs where no mailbox exists, a queued one counts,
// and an unreachable mailbox is unknown. Without an actor or writer nothing reads as zero.
#[tokio::test]
async fn the_turn_comes_from_the_mailbox_and_nothing_unknown_reads_as_zero() {
    const IDLE: u64 = 9_100_000_000_000_001;
    const QUEUED: u64 = 9_100_000_000_000_002;
    const QUIET: u64 = 9_100_000_000_000_003;
    const GONE: u64 = 9_100_000_000_000_004;
    let port = |channel| ChannelHomePort::new(channel, Arc::new(Readiness::default()));
    let registry = ChannelMailboxRegistry::default();
    let queued = ChannelMailboxSnapshot {
        intervention_queue: vec![ChannelMailboxRegistry::queued_for_test(1)],
        ..ChannelMailboxSnapshot::default()
    };
    let refusal = || MailboxRefusal::Unreachable;
    registry.insert_snapshot_only_for_test(ChannelId::new(QUEUED), queued, refusal());
    let quiet = ChannelMailboxSnapshot::default();
    registry.insert_snapshot_only_for_test(ChannelId::new(QUIET), quiet, refusal());
    registry.insert_unreachable_for_test(ChannelId::new(GONE));

    assert_eq!(port(IDLE).turn_running().await, Some(false));
    assert_eq!(port(QUEUED).turn_running().await, Some(true));
    assert_eq!(port(QUIET).turn_running().await, Some(false));
    assert_eq!(port(GONE).turn_running().await, None);
    assert_eq!(port(IDLE).owed().await, None, "no actor");
    assert_eq!(port(IDLE).posts_in_flight().await, None, "no writer");
    let reset = port(IDLE).reset_legacy_source().await;
    assert_eq!(reset, Err(ResetRefused::NotWired));
    for channel in [QUEUED, QUIET, GONE] {
        registry.remove_fixture_for_test(ChannelId::new(channel));
    }
}

fn hosted_owing(ready: &Readiness, channel: u64) -> Owing {
    let owing = Owing::default();
    let published = owing.published.subscribe();
    let demand = owing.demand.clone();
    ready.track_owed_for_test(channel, OwedView { published, demand });
    owing
}

/// Lets a spawned read run until it waits on the next send.
async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

// A read waiting when the actor sends its last value and ends reads unknown, not owed nothing;
// from a running actor the same send is the answer. The read wants the store read only meanwhile.
#[tokio::test]
async fn a_last_send_from_an_ending_actor_reads_as_unknown() {
    const ENDING: u64 = 9_100_000_000_000_005;
    const RUNNING: u64 = 9_100_000_000_000_006;
    let ready = Arc::new(Readiness::default());
    let port = |channel| ChannelHomePort::new(channel, Arc::clone(&ready));

    let owing = hosted_owing(&ready, ENDING);
    let reader = port(ENDING);
    let read = tokio::spawn(async move { reader.owed().await });
    settle().await;
    assert!(owing.demand.wanted(), "the waiting read wants it");
    owing.published.send_replace(Some(Undelivered::default()));
    let demand = owing.demand.clone();
    drop(owing);
    assert_eq!(
        read.await.unwrap(),
        None,
        "the actor ended after its last send"
    );
    assert!(!demand.wanted());

    let owing = hosted_owing(&ready, RUNNING);
    let reader = port(RUNNING);
    let read = tokio::spawn(async move { reader.owed().await });
    settle().await;
    let one = Undelivered {
        owed: 1,
        ..Undelivered::default()
    };
    owing.published.send_replace(Some(one));
    let expected = Owed {
        owed: 1,
        ..Owed::default()
    };
    assert_eq!(read.await.unwrap(), Some(expected));
    assert!(!owing.demand.wanted(), "the read ended");
}

/// The writer host needs the O store, which only a platform with directory fsync enables.
#[cfg(unix)]
mod hosted {
    use std::io::ErrorKind::StorageFull;
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::cutover::test_override;
    use crate::services::tui_o::ownership::OwnershipGate;
    use crate::services::tui_o::shadow::{ShadowProvider, binding_reader::source_id_for};
    use crate::services::tui_o::store::fault::{self, Keep, Step as At};
    use crate::services::tui_o::writer::WriterAlarm;
    use crate::services::tui_o::writer::actor::undelivered_reads_for_test;
    use crate::services::tui_o::writer::host::{self, HostParts, test_io::TestHost};

    const O: u64 = 1_490_141_479_707_086_938;

    fn row(id: &str, text: &str) -> Vec<u8> {
        let row = serde_json::json!({
            "type": "assistant", "uuid": format!("u-{id}"), "apiBlockIndex": 0,
            "message": {"id": id, "content": [{"type": "text", "text": text}]},
        });
        let mut line = serde_json::to_vec(&row).unwrap();
        line.push(b'\n');
        line
    }

    fn append(path: &PathBuf, bytes: &[u8]) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(bytes).unwrap();
    }

    fn owed(owed: usize) -> Option<Owed> {
        Some(Owed {
            owed,
            ..Owed::default()
        })
    }

    /// A first activation of `O` on a Herdr-configured gateway, hosted through the real writer host.
    struct Scene {
        runtime: tempfile::TempDir,
        transcript: PathBuf,
        io: Arc<TestHost>,
        gate: Arc<OwnershipGate>,
        ready: Arc<Readiness>,
        hosts: Vec<tokio::task::JoinHandle<()>>,
    }

    impl Drop for Scene {
        fn drop(&mut self) {
            self.hosts.iter().for_each(|host| host.abort());
        }
    }

    impl Scene {
        async fn hosted() -> Self {
            let runtime = tempfile::tempdir().unwrap();
            let transcript = runtime.path().join("o.jsonl");
            std::fs::write(&transcript, b"").unwrap();
            let io = TestHost::new([(O, source_id_for("s1", &transcript).unwrap())]);
            let (gate, ready) = (Arc::new(OwnershipGate::default()), Arc::default());
            let parts = || HostParts {
                io: Arc::clone(&io),
                runtime_root: Some(runtime.path().to_path_buf()),
                gate: Arc::clone(&gate),
                readiness: Arc::clone(&ready),
            };
            let hosts = host::start(ShadowProvider::Claude, true, parts);
            gate.acquired();
            let scene = Self {
                runtime,
                transcript,
                io,
                gate,
                ready,
                hosts,
            };
            scene.until(|scene| scene.ready.accepts(O)).await;
            scene
        }

        async fn until(&self, done: impl Fn(&Self) -> bool) {
            for _ in 0..5_000 {
                if done(self) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("never reached");
        }

        fn halted(&self) -> bool {
            let raised = self.io.alarms.0.lock().unwrap();
            raised.iter().any(|(channel, alarm)| {
                *channel == O && matches!(alarm, WriterAlarm::Halted { .. })
            })
        }

        fn spool(&self) -> PathBuf {
            let store = self.runtime.path().join("o_store");
            store.join(O.to_string()).join("spool")
        }
    }

    // A writer halted by a store error has no projection, so the drain reads its owed pieces and the
    // rotation as unknown; the in-process resume publishes the recovered actor's real values again.
    #[tokio::test(start_paused = true)]
    async fn a_halted_writer_reads_as_unknown_until_its_resume_publishes_its_real_projection() {
        let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(O, "mini")]);
        let _selected = test_override::force_channels(&[(O, ClaudeTui)]);
        let _pending = test_override::force_candidates(&[(O, ClaudeTui)]);
        let scene = Scene::hosted().await;
        let port = ChannelHomePort::new(O, Arc::clone(&scene.ready));
        assert_eq!(port.owed().await, owed(0));
        assert_eq!(scene.ready.rotation_unsettled(O), Some(0));
        assert_eq!(port.posts_in_flight().await, Some(0));

        scene.gate.lost();
        append(&scene.transcript, &row("m1", "one"));
        assert_eq!(port.owed().await, owed(1), "held while not Owned");
        let full = fault::plant(&scene.spool(), At::Append(Keep::Nothing), StorageFull, None);
        append(&scene.transcript, &row("m2", "two"));
        scene.until(Scene::halted).await;
        assert_eq!(
            port.owed().await,
            None,
            "a halted writer owes an unknown amount"
        );
        assert_eq!(scene.ready.rotation_unsettled(O), None);

        drop(full);
        tokio::time::sleep(Duration::from_secs(35)).await;
        assert_eq!(port.owed().await, owed(2), "the resumed actor's real count");
        assert_eq!(scene.ready.rotation_unsettled(O), Some(0));
        scene.gate.acquired();
        scene.until(|scene| scene.io.posts.to(O).len() == 2).await;
        assert_eq!(scene.io.posts.to(O), ["one", "two"]);
        assert_eq!(port.owed().await, owed(0));
        assert_eq!(port.posts_in_flight().await, Some(0));
    }

    // A hosted Herdr actor reads nothing for what it owes while no drain asks; a drain's read makes
    // it read and publish, and once that read returns it reads nothing again.
    #[tokio::test(start_paused = true)]
    async fn a_hosted_herdr_actor_reads_what_it_owes_only_while_a_drain_waits() {
        let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(O, "mini")]);
        let _selected = test_override::force_channels(&[(O, ClaudeTui)]);
        let _pending = test_override::force_candidates(&[(O, ClaudeTui)]);
        let scene = Scene::hosted().await;
        append(&scene.transcript, &row("m1", "one"));
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(scene.io.posts.to(O), ["one"]);
        assert_eq!(undelivered_reads_for_test(), 0, "no drain asks");

        let port = ChannelHomePort::new(O, Arc::clone(&scene.ready));
        assert_eq!(port.owed().await, owed(0));
        let read = undelivered_reads_for_test();
        assert!(read >= 1, "read while the drain waited");
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(
            undelivered_reads_for_test(),
            read,
            "nothing read after it returned"
        );
    }
}
