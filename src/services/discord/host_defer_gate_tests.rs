//! Stored-row cases and a scripted tmux the router, idle and resume host-check tests share.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use crate::db::dispatched_sessions::hosted_execution::tests::{owner, pending, wire};
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::host_teardown_gate::test_support::{
    Stored, channel_key, seed, shared_on,
};
use crate::services::discord::{DiscordSession, SharedData, inflight};
use crate::services::provider::ProviderKind;

/// What the stored rows say about one session, as a host-checked caller reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Case {
    Stored(Stored),
    /// A row whose hosted record names another channel's owner.
    Conflict,
}

impl Case {
    pub(crate) const ALL: [Self; 7] = [
        Self::Stored(Stored::Legacy),
        Self::Stored(Stored::Hosted),
        Self::Stored(Stored::Future),
        Self::Stored(Stored::Missing),
        Self::Stored(Stored::MissingHerdrMarker),
        Self::Stored(Stored::LegacyHerdrMarker),
        Self::Conflict,
    ];

    /// A found legacy row, or no row yet with no other trace, keeps main's path.
    pub(crate) fn admitted(self) -> bool {
        matches!(self, Self::Stored(Stored::Legacy | Stored::Missing))
    }

    /// Whether a sessions row exists for the case.
    pub(crate) fn has_row(self) -> bool {
        !matches!(
            self,
            Self::Stored(Stored::Missing | Stored::MissingHerdrMarker)
        )
    }

    pub(crate) async fn seed(self, pool: &PgPool, key: &str, tmux_name: &str, channel_id: u64) {
        match self {
            Self::Stored(stored) => seed(pool, key, tmux_name, channel_id, stored).await,
            Self::Conflict => {
                let foreign = wire(&pending(&owner("1"), "n9"));
                inflight::seed_session_row_keyed(pool, key, channel_id, Some(foreign)).await;
            }
        }
    }
}

pub(crate) async fn postgres() -> (crate::db::auto_queue::test_support::TestPostgresDb, PgPool) {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    (db, pool)
}

/// A claude runtime on `pool` whose registry also holds a second claude bot hashed `other`.
pub(crate) async fn with_second_bot(
    pool: &PgPool,
    other: &str,
) -> (Arc<SharedData>, Arc<HealthRegistry>) {
    let mut shared = shared_on(pool).await;
    let registry = Arc::new(HealthRegistry::new());
    let own = Arc::get_mut(&mut shared).expect("an unshared runtime");
    own.health_registry = Arc::downgrade(&registry);
    let mut second = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut second)
        .expect("an unshared runtime")
        .token_hash = other.to_string();
    registry
        .register("claude".to_string(), shared.clone())
        .await;
    registry.register("claude".to_string(), second).await;
    (shared, registry)
}

/// Maps `channel` to `channel_name` in the runtime's session table.
pub(crate) async fn map_channel(shared: &SharedData, channel: ChannelId, channel_name: &str) {
    let session = DiscordSession {
        session_id: None,
        memento_context_loaded: false,
        memento_reflected: false,
        current_path: None,
        history: Vec::new(),
        pending_uploads: Vec::new(),
        cleared: false,
        remote_profile_name: None,
        channel_id: Some(channel.get()),
        channel_name: Some(channel_name.to_string()),
        category_name: None,
        last_active: tokio::time::Instant::now(),
        worktree: None,
        born_generation: shared.restart.current_generation,
    };
    shared.core.lock().await.sessions.insert(channel, session);
}

/// PATH-first tmux logging each call: no session exists, `list-sessions` prints the
/// listed names, and with probes failing every other call is a transport error.
pub(crate) struct ScriptedTmux {
    dir: tempfile::TempDir,
    _env: crate::config::TestEnvVarGuard,
}

impl ScriptedTmux {
    /// Needs the shared test-env lock held, e.g. by a `TestRuntimeRootGuard`.
    pub(crate) fn install() -> Self {
        let dir = tempfile::TempDir::new().expect("tmux dir");
        let binary = dir.path().join("tmux");
        let mut file = std::fs::File::create(&binary).expect("scripted tmux");
        writeln!(
            file,
            "#!/bin/sh\n[ \"$1\" = -u ] && shift\nd=\"$(dirname \"$0\")\"\n\
             echo \"$*\" >> \"$d/calls\"\n\
             [ \"$1\" = list-sessions ] && {{ cat \"$d/listed\" 2>/dev/null; exit 0; }}\n\
             [ -f \"$d/fail\" ] && {{ echo 'error connecting to socket' >&2; exit 1; }}\n\
             echo \"can't find session: $3\" >&2; exit 1"
        )
        .expect("scripted tmux body");
        let permissions = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&binary, permissions).unwrap();
        let mut paths = vec![dir.path().to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let path = std::env::join_paths(paths).expect("join PATH");
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let env = set("PATH", std::path::Path::new(&path));
        Self { dir, _env: env }
    }

    pub(crate) fn list(&self, names: &[&str]) {
        let listed: String = names.iter().map(|name| format!("{name}\n")).collect();
        std::fs::write(self.dir.path().join("listed"), listed).unwrap();
    }

    pub(crate) fn fail_probes(&self, fail: bool) {
        let flag = self.dir.path().join("fail");
        if fail {
            std::fs::write(flag, "").unwrap();
        } else {
            let _ = std::fs::remove_file(flag);
        }
    }

    /// The logged calls, oldest first, and clears the log.
    pub(crate) fn take_calls(&self) -> Vec<String> {
        let log = self.dir.path().join("calls");
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(log);
        calls.lines().map(str::to_string).collect()
    }
}

// The queued-turn promote gate holds a session the host guard keeps at the queue front;
// a legacy row, or a row the promoted turn has not written yet, promotes as in main.
#[tokio::test]
async fn the_promote_gate_holds_only_what_the_host_guard_keeps_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = crate::services::provider_teardown::tests::test_support::FakeTmux::install("-");
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let gate = crate::services::discord::router::hosted_tui_promote_readiness_blocked;
    let channel_of = |n: usize| ChannelId::new(1_479_671_301_387_061_000 + n as u64);
    for (n, case) in Case::ALL.into_iter().enumerate() {
        let channel = channel_of(n);
        let channel_name = format!("p4c1-promote-{n}");
        let name = provider.build_tmux_session_name(&channel_name);
        map_channel(&shared, channel, &channel_name).await;
        case.seed(&pool, &channel_key(&shared, &name), &name, channel.get())
            .await;
        let held = gate(&shared, &provider, channel).await;
        assert_eq!(held, !case.admitted(), "{case:?}");
    }
    pool.close().await;
    let unread = gate(&shared, &provider, channel_of(0)).await;
    assert!(unread, "a failed row read is not a legacy answer");
    db.drop().await;
}

// A row written under another tmux name, with no alias for the name the channel now builds,
// still holds the promotion through its channel row; a legacy or absent row promotes.
#[tokio::test]
async fn the_promote_gate_reads_the_channel_row_under_another_name_pg() {
    use crate::db::dispatched_sessions::hosted_execution::HostedState;
    use crate::db::dispatched_sessions::hosted_execution::tests::{future_schema, record};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = crate::services::provider_teardown::tests::test_support::FakeTmux::install("-");
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let gate = crate::services::discord::router::hosted_tui_promote_readiness_blocked;
    let cases = [
        Stored::Legacy,
        Stored::Hosted,
        Stored::Future,
        Stored::Missing,
    ];
    for (n, stored) in cases.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_301_387_062_000 + n as u64);
        map_channel(&shared, channel, &format!("p4c1-renamed-{n}")).await;
        let old = provider.build_tmux_session_name(&format!("p4c1-old-{n}"));
        let key = channel_key(&shared, &old);
        let mut row_owner = owner(&channel.get().to_string());
        row_owner.discord_token_hash = shared.token_hash.clone();
        let raw = match stored {
            Stored::Hosted => Some(wire(&record(&row_owner, "n1", HostedState::Bound))),
            Stored::Future => Some(future_schema(&row_owner)),
            _ => None,
        };
        if stored != Stored::Missing {
            let hash = Some(shared.token_hash.as_str());
            let seed = crate::services::discord::host_key_derivation::tests::seed_row;
            seed(&pool, "claude", hash, &key, channel.get(), raw).await;
        }
        let held = gate(&shared, &provider, channel).await;
        let expected = matches!(stored, Stored::Hosted | Stored::Future);
        assert_eq!(held, expected, "{stored:?}");
    }
    db.drop().await;
}

/// What the channel rows say to a runtime that holds no channel name for the channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Nameless {
    Legacy,
    Hosted,
    Future,
    Missing,
    LegacyHerdrMarker,
    /// Legacy rows for the channel under two registered bot hashes.
    TwoRows,
    /// A hosted row stored under the session key alone, with no bot hash for the channel.
    HostedKeyOnly,
}

impl Nameless {
    pub(crate) const ALL: [Self; 6] = [
        Self::Legacy,
        Self::Hosted,
        Self::Future,
        Self::Missing,
        Self::LegacyHerdrMarker,
        Self::TwoRows,
    ];

    /// Seeds the case's `(claude, hash, channel)` row keyed by `tmux_name`; two rows add `other`'s.
    pub(crate) async fn seed(
        self,
        pool: &PgPool,
        hash: &str,
        other: &str,
        channel: u64,
        tmux_name: &str,
    ) {
        use crate::db::dispatched_sessions::hosted_execution::HostedState;
        use crate::db::dispatched_sessions::hosted_execution::tests::{future_schema, record};
        let seed_row = crate::services::discord::host_key_derivation::tests::seed_row;
        let build = crate::services::discord::adk_session::build_namespaced_session_key;
        let mut row_owner = owner(&channel.to_string());
        row_owner.discord_token_hash = hash.to_string();
        let raw = match self {
            Self::Hosted | Self::HostedKeyOnly => {
                Some(wire(&record(&row_owner, "n1", HostedState::Bound)))
            }
            Self::Future => Some(future_schema(&row_owner)),
            _ => None,
        };
        let key = |hash: &str| build(hash, &ProviderKind::Claude, tmux_name);
        let identity = (self != Self::HostedKeyOnly).then_some(hash);
        if self != Self::Missing {
            seed_row(pool, "claude", identity, &key(hash), channel, raw).await;
        }
        if self == Self::TwoRows {
            seed_row(pool, "claude", Some(other), &key(other), channel, None).await;
        }
        if self == Self::LegacyHerdrMarker {
            let marker = crate::services::tmux_common::session_temp_path(tmux_name, "host_kind");
            std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(marker, "herdr").unwrap();
        }
    }
}

// A nameless or absent unregistered session holds the promote unless a legacy row admits it;
// no row promotes only with nothing in flight.
#[tokio::test]
async fn the_promote_gate_reads_the_channel_row_with_no_channel_name_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = crate::services::provider_teardown::tests::test_support::FakeTmux::install("-");
    let (db, pool) = postgres().await;
    let (shared, _registry) = with_second_bot(&pool, "p4c1f-second-bot").await;
    let provider = ProviderKind::Claude;
    let gate = crate::services::discord::router::hosted_tui_promote_readiness_blocked;
    let channel_of = |n: usize| ChannelId::new(1_479_671_301_387_065_000 + n as u64);
    let own = shared.token_hash.clone();
    for (n, case) in Nameless::ALL.into_iter().enumerate() {
        let channel = channel_of(n);
        let name = provider.build_tmux_session_name(&format!("p4c1f-promote-{n}"));
        map_channel(&shared, channel, "unnamed").await;
        let mut core = shared.core.lock().await;
        core.sessions.get_mut(&channel).unwrap().channel_name = None;
        drop(core);
        case.seed(&pool, &own, "p4c1f-second-bot", channel.get(), &name)
            .await;
        let held = gate(&shared, &provider, channel).await;
        let promoted = matches!(case, Nameless::Legacy | Nameless::Missing);
        assert_eq!(held, !promoted, "{case:?}");
    }
    for (n, case) in [(10, Nameless::Hosted), (11, Nameless::Legacy)] {
        let name = provider.build_tmux_session_name(&format!("p4c1f-promote-{n}"));
        case.seed(&pool, &own, "p4c1f-second-bot", channel_of(n).get(), &name)
            .await;
        let held = gate(&shared, &provider, channel_of(n)).await;
        assert_eq!(held, case != Nameless::Legacy, "no session, {case:?}");
    }
    let unkeyed = channel_of(12);
    assert!(!gate(&shared, &provider, unkeyed).await, "unkeyed, idle");
    let row = inflight::InflightTurnState::new(
        provider.clone(),
        unkeyed.get(),
        None,
        1,
        unkeyed.get() + 1,
        unkeyed.get() + 2,
        "p4c1f in flight".to_string(),
        None,
        None,
        None,
        None,
        0,
    );
    inflight::save_inflight_state_create_new(&row).expect("inflight row");
    assert!(
        gate(&shared, &provider, unkeyed).await,
        "unkeyed, in flight"
    );
    pool.close().await;
    let unread = gate(&shared, &provider, channel_of(0)).await;
    assert!(unread, "a failed row read is not a legacy answer");
    db.drop().await;
}
