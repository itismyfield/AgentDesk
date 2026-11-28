//! Stored-row cases and a scripted tmux the router, idle and resume host-check tests share.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;

use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use crate::db::dispatched_sessions::hosted_execution::test_support::{owner, pending, wire};
pub(crate) use crate::services::discord::host_teardown_gate::test_support::with_second_bot;
use crate::services::discord::host_teardown_gate::test_support::{Stored, seed};
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

pub(in crate::services::discord) fn isolated_runtime_root() -> crate::config::TestRuntimeRootGuard {
    crate::config::TestRuntimeRootGuard::new()
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
        use crate::db::dispatched_sessions::hosted_execution::test_support::{
            future_schema, record,
        };
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
