//! The replaced-server hold and owner against a fresh database, the production registry and launch
//! host of a local user_default endpoint, and a socket this test listens on as the server.
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    ExpectedExecution, HostedOwner, ProcessStamp, SourceRef,
};
use crate::services::discord::recovery_engine::herdr_row_end::owner::{
    self, RowHold, server_replaced_hold,
};
use crate::services::herdr_launch::HerdrLaunchHost;
use crate::services::session_host::ServerWitness;
use crate::services::session_host::autostart::spawn::{
    Context, ensured_on_this_thread, scoped_context,
};

#[path = "herdr_server_replaced_real_tests.rs"]
mod real;

pub(in crate::services::discord) const CHANNEL: u64 = 7_638_200_001;
/// Hex, as launch contexts name their executions.
const NONCE: &str = "7638b2000000000000000000000000aa";
const LOGICAL: &str = "replaced-server-session";

pub(in crate::services::discord) struct Fx {
    db: crate::db::auto_queue::test_support::TestPostgresDb,
    pool: sqlx::PgPool,
    shared: Arc<SharedData>,
    home: PathBuf,
    runtime_root: String,
    launch: HerdrLaunchEndpoint,
    _dir: Option<tempfile::TempDir>,
    _hosts: crate::config::session_hosts::ForcedSessionHosts,
    _registry: Box<dyn std::any::Any>,
    _context: Box<dyn std::any::Any>,
}

impl Fx {
    pub(in crate::services::discord) async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        std::fs::create_dir_all(home.join(".config/herdr")).unwrap();
        // No binary is found, so a start this test asks for is refused before any spawn.
        let context = Context {
            env: crate::services::session_host::autostart::Environment {
                home: Some(home.clone()),
                path: None,
                excluded: vec![home.clone(), "/opt/homebrew".into(), "/usr/local".into()],
            },
            local_node: Some("node".into()),
            records: None,
            grace_rounds: 0,
            lock_wait: Duration::from_millis(200),
            ready_wait: Duration::from_millis(200),
        };
        let mut fx = Self::at(home, context, "/fixture/runtime".into()).await;
        fx._dir = Some(dir);
        fx
    }

    /// The user_default endpoint of `home`'s default socket, with `context` for any start.
    async fn at(home: PathBuf, context: Context, runtime_root: String) -> Self {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let mut shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        Arc::get_mut(&mut shared).unwrap().token_hash = "replaced-token".into();
        let socket = home.join(".config/herdr/herdr.sock");
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "server": {},
            "cluster": {"instance_id": "node"},
            "agents": [{"id": "replaced", "name": "Replaced", "channels": {
                "claude": {"id": CHANNEL.to_string(), "runtime": "tui"}
            }}],
            "session_hosts": {"herdr": {
                "endpoints": {"user": {
                    "mode": "user_default",
                    "execution_node": "node",
                    "socket_path": socket,
                    "herdr_home": "",
                    "herdr_session": "default"
                }},
                "channels": {CHANNEL.to_string(): "user"}
            }}
        }))
        .unwrap();
        let hosts = crate::config::session_hosts::force_config_for_test(&config);
        let boot = crate::config::session_hosts::BootSessionHosts::from_config(&config).unwrap();
        let registry =
            crate::services::session_host::herdr_socket_rig_tests::boot_registry_on_this_thread(
                &boot,
            );
        let context = scoped_context(&socket, context);
        let launch = HerdrLaunchEndpoint {
            execution_node: "node".into(),
            config_key: "user".into(),
            socket_addr: socket.display().to_string(),
            herdr_session: "default".into(),
        };
        Self {
            db,
            pool,
            shared,
            home,
            runtime_root,
            launch,
            _dir: None,
            _hosts: hosts,
            _registry: Box::new(registry),
            _context: Box::new(context),
        }
    }

    pub(in crate::services::discord) fn shared(&self) -> &Arc<SharedData> {
        &self.shared
    }

    pub(in crate::services::discord) fn socket_addr(&self) -> String {
        self.launch.socket_addr.clone()
    }

    fn socket(&self) -> PathBuf {
        PathBuf::from(&self.launch.socket_addr)
    }

    /// The channel's Bound row on the endpoint's pane.
    pub(in crate::services::discord) async fn bound(&self) {
        self.insert(|_| {}).await;
    }

    /// The channel's row: Bound on pane `w1-1` of the endpoint, as `edit` leaves it.
    async fn insert(&self, edit: impl FnOnce(&mut HostedExecution)) {
        let channel = CHANNEL.to_string();
        let owner = HostedOwner {
            provider: "claude".into(),
            discord_token_hash: self.shared.token_hash.clone(),
            channel_id: channel.clone(),
            logical_key: LOGICAL.into(),
            owner_node: "node".into(),
            runtime_root: self.runtime_root.clone(),
        };
        let stamp = |pid, start: &str| ProcessStamp {
            pid,
            start: start.into(),
        };
        let mut record = HostedExecution::pending(
            owner.clone(),
            NONCE.into(),
            SourceRef {
                runtime_root: owner.runtime_root.clone(),
                channel: channel.clone(),
                provider: "claude".into(),
                logical_key: owner.logical_key.clone(),
                execution_nonce: NONCE.into(),
                initial_source: None,
                baseline_event_seq: None,
            },
        );
        record.state = HostedState::Bound;
        record.location = Some(HostedLocation {
            host: "herdr".into(),
            execution_node: "node".into(),
            endpoint_config_key: "user".into(),
            socket_addr: self.launch.socket_addr.clone(),
            named_session: "default".into(),
            pane_id: "w1-1".into(),
        });
        record.expected = Some(ExpectedExecution {
            binding_provider: "claude".into(),
            binding_nonce: NONCE.into(),
            root: stamp(10, "darwin:1001.000000"),
            provider_process: stamp(20, "darwin:1002.000000"),
            provenance: "herdr_launch:ppid+env;exec=?".into(),
        });
        edit(&mut record);
        sqlx::query("INSERT INTO sessions (session_key,provider,status,identity_kind,discord_token_hash,channel_id,hosted_execution) VALUES ($1,'claude','idle','discord_channel',$2,$3,$4)")
            .bind(&owner.logical_key).bind(&self.shared.token_hash).bind(channel)
            .bind(serde_json::to_value(record).unwrap())
            .execute(&self.pool).await.unwrap();
    }

    /// The server this test answers as, read by the launch host against a grant naming another.
    pub(in crate::services::discord) fn current(&self) -> ServerWitness {
        self.grant(ServerWitness {
            socket: self.socket(),
            pid: 1,
            start: crate::services::session_host::StartIdentity::Darwin {
                seconds: 1,
                micros: 0,
            },
        });
        let host = crate::services::session_host::herdr_endpoints()
            .launch_host()
            .unwrap();
        match host.granted_server(&self.launch, NONCE) {
            GrantedServer::Replaced(server) => server.current,
            other => panic!("the listening socket is no server: {other:?}"),
        }
    }

    pub(in crate::services::discord) fn grant(&self, witness: ServerWitness) {
        use crate::services::session_host::policy::permit::grant_for_test;
        let host = crate::services::session_host::herdr_endpoints()
            .launch_host()
            .unwrap();
        host.remember_grant(grant_for_test(&self.launch, NONCE, witness));
    }

    async fn hold(&self) -> Option<RowHold> {
        let channel = ChannelId::new(CHANNEL);
        server_replaced_hold(&self.shared, &ProviderKind::Claude, channel).await
    }

    async fn row(&self) -> Option<HostedObservation> {
        let channel = ChannelId::new(CHANNEL);
        canonical_row(
            &self.pool,
            &self.shared.token_hash,
            &ProviderKind::Claude,
            channel,
        )
        .await
    }

    pub(in crate::services::discord) async fn close(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

async fn asked_since(before: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(5);
    while ensured_on_this_thread() == before && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    ensured_on_this_thread() - before
}

/// Only witnesses decide: a replaced or absent server holds (an absent one is asked for), the
/// granted server never holds, and an unjudgeable socket keeps the existing path.
#[tokio::test(flavor = "current_thread")]
async fn a_row_is_held_only_while_its_granted_server_is_replaced_or_absent_pg() {
    let fx = Fx::new().await;
    fx.bound().await;
    let listener = UnixListener::bind(fx.socket()).unwrap();
    let current = fx.current();
    let other = ServerWitness {
        pid: current.pid + 1,
        ..current.clone()
    };
    fx.grant(other);
    assert_eq!(fx.hold().await, Some(RowHold::ServerReplaced));
    // This socket answers no hello, so its policy cannot pass; the granted server still runs.
    fx.grant(current);
    assert_eq!(fx.hold().await, None, "the granted server never holds");

    drop(listener);
    std::fs::remove_file(fx.socket()).unwrap();
    let before = ensured_on_this_thread();
    assert_eq!(fx.hold().await, Some(RowHold::ServerReplaced), "absent");
    assert_eq!(
        asked_since(before).await,
        1,
        "an absent server is asked for"
    );

    std::fs::remove_dir(fx.home.join(".config/herdr")).unwrap();
    let before = ensured_on_this_thread();
    assert_eq!(fx.hold().await, None, "a socket that cannot be judged");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(ensured_on_this_thread(), before);
    fx.close().await;
}

/// The automatic dequeue keeps the input queued behind a replaced server and dequeues as before
/// behind the granted one.
#[tokio::test(flavor = "current_thread")]
async fn the_dequeue_holds_only_behind_a_replaced_server_pg() {
    let fx = Fx::new().await;
    fx.bound().await;
    let _listener = UnixListener::bind(fx.socket()).unwrap();
    let current = fx.current();
    let channel = ChannelId::new(CHANNEL);
    fx.grant(current.clone());
    assert!(!owner::hold_queue(&fx.shared, &ProviderKind::Claude, channel).await);
    fx.grant(ServerWitness {
        pid: current.pid + 1,
        ..current
    });
    assert!(owner::hold_queue(&fx.shared, &ProviderKind::Claude, channel).await);
    fx.close().await;
}

/// The owner retires nothing until the server answering now has passed its policy read: a start
/// that was refused, or a policy that does not read, leaves the row as it was.
#[tokio::test(flavor = "current_thread")]
async fn the_owner_retires_nothing_without_the_new_server_policy_pg() {
    let fx = Fx::new().await;
    fx.bound().await;
    let channel = ChannelId::new(CHANNEL);
    let before = fx.row().await.unwrap();
    let refused = recover(&fx.shared, &ProviderKind::Claude, channel).await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.starts_with("server_unavailable")),
        "{refused:?}"
    );
    let _listener = UnixListener::bind(fx.socket()).unwrap();
    let current = fx.current();
    fx.grant(ServerWitness {
        pid: current.pid + 1,
        ..current
    });
    let unread = recover(&fx.shared, &ProviderKind::Claude, channel).await;
    assert!(
        unread.as_ref().is_err_and(|e| e.starts_with("policy:")),
        "{unread:?}"
    );
    assert_eq!(fx.row().await, Some(before), "the row is kept");
    fx.close().await;
}
