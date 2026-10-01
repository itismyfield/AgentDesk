use std::sync::Mutex;

use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedExecution;
use crate::db::dispatched_sessions::hosted_execution::tests::{location, owner, record};
use crate::services::discord::tmux::execution_identity::herdr_observation::HerdrMismatch;
use HerdrExecutionMatch::{Match, Mismatch, Unknown};

/// Answers every read with one scripted reading and records which panes it was asked for.
struct Reader {
    endpoint: Option<HerdrEndpointId>,
    reading: HerdrPaneReading,
    reads: Mutex<Vec<String>>,
}

impl Reader {
    fn new(reading: HerdrPaneReading) -> Self {
        Self {
            endpoint: Some(HerdrEndpointId::of(&location("pane-1"))),
            reading,
            reads: Mutex::default(),
        }
    }

    fn reads(&self) -> Vec<String> {
        self.reads.lock().unwrap().clone()
    }
}

impl HerdrExecutionReader for Reader {
    fn endpoint(&self) -> Option<&HerdrEndpointId> {
        self.endpoint.as_ref()
    }

    fn read_pane(&self, pane_id: &str) -> HerdrPaneReading {
        self.reads.lock().unwrap().push(pane_id.to_string());
        self.reading.clone()
    }
}

fn bound() -> HostedExecution {
    record(&owner("100"), "n1", HostedState::Bound)
}

/// What the stored execution's pane shows while that execution still runs.
fn running(stored: &HostedExecution) -> HerdrPaneEvidence {
    let expected = stored.expected.clone().unwrap();
    HerdrPaneEvidence {
        binding_nonce: Some(stored.execution_nonce.clone()),
        root: Some(expected.root),
        provider_process: Some(expected.provider_process),
        agent_session_id: Some("agent-a".into()),
    }
}

fn mark(logical_key: &str, host: Option<&str>) {
    let path = crate::services::tmux_common::session_temp_path(logical_key, "host_kind");
    let _ = std::fs::remove_file(&path);
    if let Some(host) = host {
        std::fs::write(&path, host).unwrap();
    }
}

// Each row of the restore table: only an unchanged root, provider and nonce on the stored
// endpoint and pane is a match; nothing is read off that endpoint or beside that pane.
#[test]
fn herdr_restart_reconcile_follows_the_restore_table() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let stored = bound();
    let key = stored.owner.logical_key.clone();
    let present = |change: fn(&mut HerdrPaneEvidence)| {
        let mut evidence = running(&stored);
        change(&mut evidence);
        HerdrPaneReading::Present(evidence)
    };
    type Row = (
        &'static str,
        Option<&'static str>,
        HerdrPaneReading,
        HostReconcile,
    );
    let herdr = Some("herdr");
    let rows: Vec<Row> = vec![
        (
            "unchanged, read again",
            herdr,
            present(|_| {}),
            HostReconcile::Herdr(Match),
        ),
        (
            "root replaced",
            herdr,
            present(|e| e.root.as_mut().unwrap().pid += 9),
            HostReconcile::Herdr(Mismatch(HerdrMismatch::RootReplaced)),
        ),
        (
            "provider replaced under the same native session",
            herdr,
            present(|e| e.provider_process.as_mut().unwrap().start = "1700009999".into()),
            HostReconcile::Herdr(Mismatch(HerdrMismatch::ProviderReplaced)),
        ),
        (
            "another nonce",
            herdr,
            present(|e| e.binding_nonce = Some("n2".into())),
            HostReconcile::Herdr(Mismatch(HerdrMismatch::OtherNonce)),
        ),
        (
            "only the agent session differs",
            herdr,
            present(|e| e.agent_session_id = Some("agent-b".into())),
            HostReconcile::Herdr(Match),
        ),
        (
            "nonce unread, agent session agrees",
            herdr,
            present(|e| e.binding_nonce = None),
            HostReconcile::Herdr(Unknown(HerdrUnknown::NotObserved)),
        ),
        (
            "root replaced, marker lost",
            None,
            present(|e| e.root.as_mut().unwrap().pid += 9),
            HostReconcile::Herdr(Mismatch(HerdrMismatch::RootReplaced)),
        ),
        (
            "unchanged, marker lost",
            None,
            present(|_| {}),
            HostReconcile::Herdr(Unknown(HerdrUnknown::MarkerLost)),
        ),
        (
            "unchanged, marker names tmux",
            Some("tmux"),
            present(|_| {}),
            HostReconcile::Herdr(Mismatch(HerdrMismatch::OtherHostMarker)),
        ),
        (
            "read failed",
            herdr,
            HerdrPaneReading::Unreadable("timeout".into()),
            HostReconcile::Herdr(Unknown(HerdrUnknown::ProbeFailed)),
        ),
        (
            "complete snapshot without the pane",
            herdr,
            HerdrPaneReading::Missing,
            HostReconcile::Missing,
        ),
    ];
    for (name, marker, reading, verdict) in rows {
        mark(&key, marker);
        let reader = Reader::new(reading);
        let got = reconcile_record(&HostedRecord::Known(stored.clone()), &reader);
        assert_eq!(got, verdict, "{name}");
        assert_eq!(
            got.admits_reconnect(),
            verdict == HostReconcile::Herdr(Match)
        );
        assert_eq!(reader.reads(), ["pane-1"], "{name}: only the stored pane");
    }

    mark(&key, herdr);
    let unchanged = || HerdrPaneReading::Present(running(&stored));
    let mut launched = stored.clone();
    launched.state = HostedState::Pending;
    let got = reconcile_record(&HostedRecord::Known(launched), &Reader::new(unchanged()));
    assert_eq!(got, HostReconcile::Pending(Match), "launched, never bound");
    assert!(
        !got.admits_reconnect(),
        "a pending launch is never reconnected"
    );
    let mut elsewhere = Reader::new(HerdrPaneReading::Missing);
    elsewhere.endpoint.as_mut().unwrap().socket_addr = "/adk/other.sock".into();
    let mut pending = stored.clone();
    (pending.state, pending.expected) = (HostedState::Pending, None);
    let retired = record(&owner("100"), "n1", HostedState::Retired);
    let unread: Vec<(&str, HostedRecord, Reader, HostReconcile)> = vec![
        (
            "another endpoint answers for the pane id",
            HostedRecord::Known(stored.clone()),
            elsewhere,
            HostReconcile::Herdr(Unknown(HerdrUnknown::EndpointChanged)),
        ),
        (
            "no endpoint configured",
            HostedRecord::Known(stored.clone()),
            Reader {
                endpoint: None,
                ..Reader::new(unchanged())
            },
            HostReconcile::Herdr(Unknown(HerdrUnknown::ProbeFailed)),
        ),
        (
            "pending pane without launch evidence",
            HostedRecord::Known(pending),
            Reader::new(unchanged()),
            HostReconcile::Pending(Unknown(HerdrUnknown::NoStoredEvidence)),
        ),
        (
            "retired",
            HostedRecord::Known(retired),
            Reader::new(unchanged()),
            HostReconcile::Unresolved("retired hosted record".into()),
        ),
        (
            "legacy row",
            HostedRecord::Legacy,
            Reader::new(unchanged()),
            HostReconcile::Legacy,
        ),
        (
            "unreadable record",
            HostedRecord::Unknown(serde_json::json!({"schema": 2})),
            Reader::new(unchanged()),
            HostReconcile::Unresolved("unreadable hosted record".into()),
        ),
    ];
    for (name, stored, reader, verdict) in unread {
        let got = reconcile_record(&stored, &reader);
        assert_eq!(got, verdict, "{name}");
        assert!(!got.admits_reconnect(), "{name}");
        assert_eq!(reader.reads(), Vec::<String>::new(), "{name}: nothing read");
    }
    let unconfigured = reconcile_record(&HostedRecord::Known(bound()), &NoHerdrEndpoint);
    assert_eq!(
        unconfigured,
        HostReconcile::Herdr(Unknown(HerdrUnknown::ProbeFailed))
    );
}

// The outer reconcile on real rows: every verdict leaves the row byte-identical, never
// stores what it observed as the expectation, and calls no tmux.
#[cfg(unix)]
#[tokio::test]
async fn herdr_restart_reconcile_reads_the_row_and_changes_nothing_pg() {
    use crate::db::dispatched_sessions::hosted_execution::tests::{TOKEN, wire};
    use crate::services::discord::host_defer_gate::tests::ScriptedTmux;
    use crate::services::provider::ProviderKind;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ScriptedTmux::install();
    let (db, pool) = crate::services::discord::host_defer_gate::tests::postgres().await;
    let seed = crate::services::discord::host_key_derivation::tests::seed_row;
    let key_of = |n: u64| {
        let name = format!("AgentDesk-claude-p7r-reconcile-{n}");
        let claude = ProviderKind::Claude;
        crate::services::discord::adk_session::build_namespaced_session_key(TOKEN, &claude, &name)
    };
    let replaced = |stored: &HostedExecution| {
        let mut evidence = running(stored);
        evidence.root.as_mut().unwrap().pid += 9;
        HerdrPaneReading::Present(evidence)
    };
    let pending_with_pane = |owner: &_| {
        let mut pending = record(owner, "n1", HostedState::Pending);
        pending.location = Some(location("pane-1"));
        pending
    };
    let cases: Vec<(
        Option<HostedExecution>,
        fn(&HostedExecution) -> HerdrPaneReading,
        HostReconcile,
    )> = vec![
        (
            Some(bound()),
            replaced,
            HostReconcile::Herdr(Mismatch(HerdrMismatch::RootReplaced)),
        ),
        (
            Some(bound()),
            |_| HerdrPaneReading::Missing,
            HostReconcile::Missing,
        ),
        (
            Some(bound()),
            |_| HerdrPaneReading::Unreadable("timeout".into()),
            HostReconcile::Herdr(Unknown(HerdrUnknown::ProbeFailed)),
        ),
        (
            Some(pending_with_pane(&owner("100"))),
            |_| HerdrPaneReading::Present(running(&bound())),
            HostReconcile::Pending(Unknown(HerdrUnknown::NoStoredEvidence)),
        ),
        (None, |_| HerdrPaneReading::Missing, HostReconcile::Legacy),
    ];
    for (n, (stored, reading, verdict)) in cases.into_iter().enumerate() {
        let channel = 1_479_671_301_387_069_000 + n as u64;
        let stored = stored.map(|mut stored| {
            stored.owner = owner(&channel.to_string());
            stored.source_ref.channel = channel.to_string();
            stored
        });
        let key = key_of(n as u64);
        mark(&owner("100").logical_key, Some("herdr"));
        seed(
            &pool,
            "claude",
            Some(TOKEN),
            &key,
            channel,
            stored.as_ref().map(wire),
        )
        .await;
        let lookup = HostedLookupKey::SessionKey(&key);
        let before = load_hosted_execution_pg(&pool, lookup).await;
        let reader = Reader::new(stored.as_ref().map_or(HerdrPaneReading::Missing, reading));
        let got = reconcile_hosted_session_pg(&pool, lookup, &reader).await;
        assert_eq!(got, verdict, "case {n}");
        assert!(!got.admits_reconnect(), "case {n}");
        assert_eq!(
            load_hosted_execution_pg(&pool, lookup).await,
            before,
            "case {n}"
        );
        let expected_reads = usize::from(matches!(&stored, Some(s) if s.expected.is_some()));
        assert_eq!(reader.reads().len(), expected_reads, "case {n}");
    }
    let absent = key_of(99);
    let lookup = HostedLookupKey::SessionKey(&absent);
    let reader = Reader::new(HerdrPaneReading::Missing);
    let got = reconcile_hosted_session_pg(&pool, lookup, &reader).await;
    assert_eq!(
        got,
        HostReconcile::Unresolved("Missing".into()),
        "no row is not legacy"
    );
    assert_eq!(reader.reads(), Vec::<String>::new());
    assert_eq!(tmux.take_calls(), Vec::<String>::new(), "no tmux fallback");
    pool.close().await;
    db.drop().await;
}
