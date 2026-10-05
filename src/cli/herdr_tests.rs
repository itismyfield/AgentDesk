//! `agentdesk herdr` against a PG row and an in-process Herdr server: retire only on a read that
//! shows the execution ended, its hold released only after the nonce CAS, and a read-only status.
#![cfg(unix)]

use std::any::Any;
use std::path::PathBuf;

use serde_json::{Value, json};
use sqlx::PgPool;

use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedOwner, HostedRecord, HostedState, SourceRef,
    list_local_herdr_rows_pg, retire_pg,
};
use crate::services::session_host::PaneReading;
use crate::services::session_host::herdr_socket_rig_tests::{
    HerdrRig, KEY, NODE, PANE, SESSION, SHELL,
};

const CHANNEL: &str = "1479671301387059400";
const TOKEN: &str = "discord_0123456789abcdef";
const NONCE: &str = "0123456789abcdef0123456789abcdef";
const OTHER: &str = "fedcba9876543210fedcba9876543210";

/// A Bound execution of `NONCE` on the rig's pane, its row on PG and this node's endpoint.
struct Node {
    rt: tokio::runtime::Runtime,
    db: Option<crate::db::auto_queue::test_support::TestPostgresDb>,
    pool: PgPool,
    rig: HerdrRig,
    context: PathBuf,
    _guards: Vec<Box<dyn Any>>,
    _root: crate::config::TestRuntimeRootGuard,
}

impl Node {
    fn new(tag: &str) -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let rig = HerdrRig::start();
        let guards: Vec<Box<dyn Any>> = vec![
            Box::new(rig.registry_on_this_thread()),
            Box::new(crate::config::session_hosts::force_for_test(
                Some(NODE),
                &[],
            )),
        ];
        let record = bound(&rig, tag);
        let (db, pool) = rt.block_on(async {
            let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            sqlx::query(
                "INSERT INTO sessions (session_key, provider, status, identity_kind,
                                       discord_token_hash, channel_id, hosted_execution)
                 VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)",
            )
            .bind(format!(
                "claude/{TOKEN}/{NODE}:{}",
                record.owner.logical_key
            ))
            .bind(TOKEN)
            .bind(CHANNEL)
            .bind(serde_json::to_value(&record).unwrap())
            .execute(&pool)
            .await
            .unwrap();
            (db, pool)
        });
        let context = rig.context(NONCE);
        rig.run_provider(&context, false);
        rig.show_panes(&[PANE]);
        Self {
            rt,
            db: Some(db),
            pool,
            rig,
            context,
            _guards: guards,
            _root: root,
        }
    }

    fn row(&self) -> Option<Value> {
        self.rt.block_on(async {
            sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id = $1")
                .bind(CHANNEL)
                .fetch_one(&self.pool)
                .await
                .unwrap()
        })
    }

    fn state(&self) -> Option<HostedState> {
        match HostedRecord::decode(self.row().as_ref()) {
            HostedRecord::Known(record) => Some(record.state),
            _ => None,
        }
    }

    fn retire(&self) -> Result<Retired, RetireRefusal> {
        let channel = CHANNEL.parse().unwrap();
        self.rt.block_on(retire(&self.pool, channel))
    }

    /// Only snapshots and process reads reached the server: nothing was written, closed or killed.
    fn wrote_nothing(&self) -> bool {
        self.rig.requests().iter().all(|request| {
            let method = request["method"].as_str().unwrap_or("");
            matches!(method, "session.snapshot" | "pane.process_info")
        })
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let (db, pool) = (self.db.take().unwrap(), self.pool.clone());
        self.rt.block_on(async {
            pool.close().await;
            db.drop().await;
        });
    }
}

fn bound(rig: &HerdrRig, tag: &str) -> HostedExecution {
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: TOKEN.into(),
        channel_id: CHANNEL.into(),
        logical_key: format!("AgentDesk-claude-p9c-{tag}"),
        owner_node: NODE.into(),
        runtime_root: "/adk/runtime".into(),
    };
    HostedExecution {
        schema: 1,
        state: HostedState::Bound,
        execution_nonce: NONCE.into(),
        location: Some(HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            named_session: SESSION.into(),
            pane_id: PANE.into(),
        }),
        expected: Some(rig.expected(NONCE)),
        source_ref: SourceRef {
            runtime_root: owner.runtime_root.clone(),
            channel: owner.channel_id.clone(),
            provider: "claude".into(),
            logical_key: owner.logical_key.clone(),
            execution_nonce: NONCE.into(),
            initial_source: None,
            baseline_event_seq: None,
        },
        owner,
    }
}

fn hold_of(nonce: &str) -> PathBuf {
    let root = crate::config::runtime_root().unwrap();
    root.join("runtime/herdr_input_holds").join(nonce)
}

/// What an unclear prompt leaves for `nonce`: the hold, recording only its time.
fn hold(nonce: &str) -> String {
    let at = "2026-10-05T07:00:00+00:00";
    std::fs::create_dir_all(hold_of(nonce).parent().unwrap()).unwrap();
    std::fs::write(hold_of(nonce), at).unwrap();
    at.to_string()
}

// T-R3: anything but a read showing the execution ended leaves the row and holds as they were; a
// pane gone from a complete snapshot retires it and releases its own hold only, once.
#[test]
fn t_r3_retire_needs_a_read_that_shows_the_execution_ended_pg() {
    let node = Node::new("retire");
    hold(NONCE);
    hold(OTHER);
    let row = node.row();
    let unchanged = |label: &str| {
        assert_eq!(node.row(), row, "{label}: row");
        assert!(
            hold_of(NONCE).exists() && hold_of(OTHER).exists(),
            "{label}: holds"
        );
    };

    assert_eq!(node.retire(), Err(RetireRefusal::ProviderRunning));
    unchanged("provider running");

    node.rig.answer("session.snapshot", json!({"type": "ok"}));
    let refused = node.retire();
    assert!(
        matches!(refused, Err(RetireRefusal::Unproven(_))),
        "{refused:?}"
    );
    unchanged("off-contract snapshot");

    node.rig.show_panes(&[PANE]);
    node.rig.restart_shell(&node.context);
    node.rig.foreground(&[SHELL]);
    assert_eq!(
        node.retire(),
        Err(RetireRefusal::Unproven("root shell replaced"))
    );
    unchanged("root shell replaced");

    node.rig.show_panes(&["w1-9"]);
    let retired = node.retire().unwrap();
    assert_eq!((retired.nonce.as_str(), retired.hold), (NONCE, Ok(())));
    assert_eq!(node.state(), Some(HostedState::Retired));
    assert!(!hold_of(NONCE).exists(), "the retired execution's hold");
    assert!(hold_of(OTHER).exists(), "another execution's hold");

    let retired_row = node.row();
    assert!(matches!(node.retire(), Err(RetireRefusal::NoRow(_))));
    assert_eq!(node.row(), retired_row);
    assert!(hold_of(OTHER).exists());
    assert!(node.wrote_nothing());
}

// A provider that exited leaves the recorded root shell alone in the pane: that retires too.
#[test]
fn a_lone_recorded_root_shell_retires_its_execution_pg() {
    let node = Node::new("exited");
    hold(NONCE);
    node.rig.foreground(&[SHELL]);
    let retired = node.retire().unwrap();
    assert_eq!((retired.nonce.as_str(), retired.hold), (NONCE, Ok(())));
    assert_eq!(node.state(), Some(HostedState::Retired));
    assert!(!hold_of(NONCE).exists());
    assert!(node.wrote_nothing());
}

// The row changed between the read and the CAS: the retire is refused and the hold stays.
#[test]
fn a_retire_whose_cas_fails_keeps_the_hold_pg() {
    let node = Node::new("stale");
    hold(NONCE);
    let rows = node
        .rt
        .block_on(list_local_herdr_rows_pg(&node.pool, NODE))
        .unwrap();
    let HostedRecord::Known(record) = &rows[0].record else {
        panic!("{rows:?}");
    };
    let fresh = rows[0].clone();
    let owner = record.owner.clone();
    node.rt
        .block_on(retire_pg(&node.pool, &fresh, &owner, NONCE))
        .unwrap();
    let after = node.row();
    let refused = node.rt.block_on(retire_on_reading(
        &node.pool,
        &rows[0],
        record,
        &PaneReading::Missing,
    ));
    assert!(
        matches!(refused, Err(RetireRefusal::Changed(_))),
        "{refused:?}"
    );
    assert!(hold_of(NONCE).exists(), "a failed CAS keeps the hold");
    assert_eq!(node.row(), after);
}

// Status shows each row's pane as read and every hold by nonce and time, without a path, and
// changes nothing.
#[test]
fn status_shows_rows_panes_and_holds_without_paths_pg() {
    let node = Node::new("status");
    let (at, other_at) = (hold(NONCE), hold(OTHER));
    let row = node.row();
    let status = node.rt.block_on(status(&node.pool)).unwrap();
    let expected = json!({
        "executions": [{"channel": CHANNEL, "provider": "claude", "state": "bound",
            "nonce": NONCE, "pane": "provider_running", "input_hold": {"recorded_at": at}}],
        "other_input_holds": [{"nonce": OTHER, "recorded_at": other_at}],
    });
    assert_eq!(status, expected);
    let text = status.to_string();
    let root = crate::config::runtime_root().unwrap();
    assert!(!text.contains(&*root.to_string_lossy()));
    assert!(!text.contains(&*node.rig.socket().to_string_lossy()));
    assert_eq!(node.row(), row);
    assert!(hold_of(NONCE).exists() && hold_of(OTHER).exists());
    assert!(node.wrote_nothing());
}
