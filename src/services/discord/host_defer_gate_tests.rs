//! Stored-row cases and a scripted tmux the router, idle and resume host-check tests share.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;

use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use crate::db::dispatched_sessions::hosted_execution::tests::{owner, pending, wire};
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
