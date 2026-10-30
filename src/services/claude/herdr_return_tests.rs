use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::*;
use crate::config::{TestEnvVarGuard, TestRuntimeRootGuard};
use crate::db::dispatched_sessions::hosted_execution::tests::{owner, record};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, HostedRecord, load_hosted_execution_pg,
};
use crate::services::tmux_common::{self as tc, host_marker};
use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
use crate::services::tui_prompt_dedupe::{herdr_listed_execution, install_herdr_execution};

const CHANNEL: u64 = 7_504;
const LOGICAL: &str = "AgentDesk-claude-return";

/// One host's earlier Claude: running while `alive`; an `/exit` ends it unless `stays`.
#[derive(Default)]
pub(crate) struct Side {
    pub(crate) alive: bool,
    unknown: bool,
    draft: bool,
    stays: bool,
    reads: usize,
    pub(crate) exits: usize,
    /// A flag the `/exit` that ends it writes, as its host sees the end.
    pub(crate) ends: Option<PathBuf>,
}

struct View(Arc<Mutex<Side>>);

/// A logical key with the Herdr and tmux Claude a production return reads instead of its hosts.
type StandIns = Vec<(String, Arc<Mutex<Side>>, Arc<Mutex<Side>>)>;

static STAND_INS: Mutex<StandIns> = Mutex::new(Vec::new());

/// `logical`'s Herdr and tmux Claude as every production return reads them while this lives.
pub(crate) struct StandIn {
    logical: String,
    pub(crate) herdr: Arc<Mutex<Side>>,
    pub(crate) tmux: Arc<Mutex<Side>>,
}

impl StandIn {
    pub(crate) fn new(logical: &str) -> Self {
        let (herdr, tmux) = (Arc::<Mutex<Side>>::default(), Arc::<Mutex<Side>>::default());
        let mut stand_ins = STAND_INS.lock().unwrap_or_else(|e| e.into_inner());
        stand_ins.retain(|(key, _, _)| key != logical);
        stand_ins.push((logical.to_owned(), Arc::clone(&herdr), Arc::clone(&tmux)));
        Self {
            logical: logical.to_owned(),
            herdr,
            tmux,
        }
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        let mut stand_ins = STAND_INS.lock().unwrap_or_else(|e| e.into_inner());
        stand_ins.retain(|(key, _, _)| *key != self.logical);
    }
}

pub(super) fn standing_in(logical: &str, herdr: bool) -> Option<Box<dyn EarlierExecution>> {
    let stand_ins = STAND_INS.lock().unwrap_or_else(|e| e.into_inner());
    let (_, on_herdr, on_tmux) = stand_ins.iter().find(|(key, _, _)| key == logical)?;
    let side = if herdr { on_herdr } else { on_tmux };
    Some(Box::new(View(Arc::clone(side))))
}

impl EarlierExecution for View {
    fn liveness(&mut self) -> Liveness {
        let mut side = self.0.lock().unwrap();
        side.reads += 1;
        match (side.unknown, side.alive) {
            (true, _) => Liveness::Unknown("probe failed".into()),
            (false, true) => Liveness::Running,
            (false, false) => Liveness::Ended,
        }
    }

    fn send_exit(&mut self) -> ExitSend {
        let mut side = self.0.lock().unwrap();
        if side.draft {
            return ExitSend::Untouched("a person's draft".into());
        }
        side.exits += 1;
        side.alive = side.stays;
        if let Some(flag) = side.ends.as_ref().filter(|_| !side.stays) {
            std::fs::write(flag, "").unwrap();
        }
        ExitSend::Sent
    }
}

#[derive(Default)]
struct Ports {
    herdr: Arc<Mutex<Side>>,
    tmux: Arc<Mutex<Side>>,
}

impl ReturnPorts for Ports {
    fn herdr<'a>(&self, _record: &'a HostedExecution) -> Box<dyn EarlierExecution + 'a> {
        Box::new(View(self.herdr.clone()))
    }

    fn tmux<'a>(&self, _logical: &'a str) -> Box<dyn EarlierExecution + 'a> {
        Box::new(View(self.tmux.clone()))
    }

    fn exit_wait(&self) -> std::time::Duration {
        std::time::Duration::from_millis(100)
    }
}

/// A channel whose Claude session runs on Herdr: its row, launch context, account marker,
/// transcript with an O cursor, Herdr listing and host marker, as a Herdr resume leaves them.
struct Fixture {
    rt: tokio::runtime::Runtime,
    db: Option<crate::db::auto_queue::test_support::TestPostgresDb>,
    pool: PgPool,
    cwd: tempfile::TempDir,
    session: String,
    nonce: String,
    key: String,
    _home: (TestEnvVarGuard, tempfile::TempDir),
    _root: TestRuntimeRootGuard,
}

impl Fixture {
    fn new() -> Self {
        let root = TestRuntimeRootGuard::new();
        let home = tempfile::tempdir().unwrap();
        let env =
            TestEnvVarGuard::set_path_after_shared_test_env_lock("CLAUDE_CONFIG_DIR", home.path());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = rt.block_on(crate::db::auto_queue::test_support::TestPostgresDb::create());
        let pool = rt.block_on(db.connect_and_migrate());
        let session = uuid::Uuid::new_v4().to_string();
        let prepared = PreparedIncarnation::prepare_at(
            "claude",
            LOGICAL,
            Some(CHANNEL),
            Some(&session),
            true,
            None,
        )
        .unwrap();
        let nonce = prepared.context.execution_nonce.clone();
        tc::write_claude_session_auth(LOGICAL, &session, "default").unwrap();
        let fx = Self {
            rt,
            db: Some(db),
            pool,
            cwd: tempfile::tempdir().unwrap(),
            session,
            nonce,
            key: format!(
                "claude/{}/test-node:{LOGICAL}",
                owner("x").discord_token_hash
            ),
            _home: (env, home),
            _root: root,
        };
        fx.seed(HostedState::Bound);
        fx.transcript(true);
        std::fs::write(tc::session_temp_path(LOGICAL, "host_kind"), "herdr").unwrap();
        install_herdr_execution(LOGICAL, &fx.nonce);
        fx
    }

    fn seed(&self, state: HostedState) {
        let mut stored = record(&owner(&CHANNEL.to_string()), &self.nonce, state);
        stored.owner.logical_key = LOGICAL.into();
        stored.source_ref.logical_key = LOGICAL.into();
        let raw = serde_json::to_value(stored).unwrap();
        let query = sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, identity_kind,
                                   discord_token_hash, channel_id, hosted_execution)
             VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)
             ON CONFLICT (session_key) DO UPDATE SET hosted_execution = EXCLUDED.hosted_execution",
        )
        .bind(&self.key)
        .bind(&owner("x").discord_token_hash)
        .bind(CHANNEL.to_string())
        .bind(raw);
        self.rt.block_on(query.execute(&self.pool)).unwrap();
    }

    /// The session's transcript, with O reading it when `cursor`.
    fn transcript(&self, cursor: bool) -> PathBuf {
        self.transcript_under(None, cursor)
    }

    /// As `transcript`, in the Claude home `home` (the daemon's when `None`).
    fn transcript_under(&self, home: Option<&std::path::Path>, cursor: bool) -> PathBuf {
        use crate::services::tui_o::store::spool::{Cursor, source_key};
        let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            self.cwd.path(),
            &self.session,
            home,
        )
        .unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}\n").unwrap();
        let dir = crate::config::runtime_root().unwrap();
        let dir = dir.join("o_store").join(CHANNEL.to_string()).join("cursor");
        let _ = std::fs::remove_dir_all(&dir);
        if cursor {
            let source = session_handoff::transcript_file(self.cwd.path(), &self.session, home)
                .unwrap()
                .unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join(format!("{}.json", source_key(&source)));
            let cursor = Cursor {
                source,
                captured_through: 3,
                prefix_hash: "00".into(),
                retired: false,
                transition_cleanup: None,
            };
            std::fs::write(file, serde_json::to_vec(&cursor).unwrap()).unwrap();
        }
        path
    }

    fn plan(&self) -> ReturnPlan {
        let HostedLookup::Found(row) = self.rt.block_on(load_hosted_execution_pg(
            &self.pool,
            HostedLookupKey::SessionKey(&self.key),
        )) else {
            panic!("no row");
        };
        let HostedRecord::Known(record) = row.record.clone() else {
            panic!("no record");
        };
        ReturnPlan { row, record }
    }

    fn state(&self) -> HostedState {
        self.plan().record.state
    }

    fn run(&self, ports: &Ports) -> Result<(), String> {
        let turn = ReturnTurn {
            pool: &self.pool,
            channel_id: CHANNEL,
            working_dir: self.cwd.path().to_str().unwrap(),
            session_id: Some(&self.session),
            force_fresh: false,
        };
        execute(&turn, &self.plan(), self.rt.handle(), ports)
    }

    fn marker(&self) -> host_marker::HostKindMarker {
        host_marker::read_host_kind_marker(LOGICAL)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(db) = self.db.take() {
            self.rt.block_on(self.pool.close());
            self.rt.block_on(db.drop());
        }
    }
}

fn running() -> Ports {
    let ports = Ports::default();
    ports.herdr.lock().unwrap().alive = true;
    ports
}

fn on_herdr(marker: host_marker::HostKindMarker) -> bool {
    let herdr = crate::services::session_host::HostKind::Herdr;
    marker == host_marker::HostKindMarker::Known(herdr)
}

// A Bound Herdr Claude ends by one `/exit`, its row retires, its listing ends and the marker
// names tmux, so the tmux turn resumes the same session; a newer listing would stay.
#[test]
fn b1_a_bound_herdr_claude_exits_once_retires_and_hands_its_session_back_pg() {
    let fx = Fixture::new();
    tc::write_claude_session_auth(LOGICAL, &fx.session, "work").unwrap();
    let ports = running();
    assert_eq!(fx.run(&ports), Ok(()));
    // The tmux turn compares its account with the one the session last ran under.
    let profile = tc::session_temp_path(LOGICAL, "auth_profile");
    assert_eq!(std::fs::read_to_string(profile).unwrap(), "work");
    assert_eq!(ports.herdr.lock().unwrap().exits, 1);
    assert_eq!(fx.state(), HostedState::Retired);
    let tmux = crate::services::session_host::HostKind::Tmux;
    assert_eq!(fx.marker(), host_marker::HostKindMarker::Known(tmux));
    assert_eq!(herdr_listed_execution(LOGICAL), None);
    assert!(crate::services::turn_host::returned(LOGICAL, &fx.nonce));
    install_herdr_execution(LOGICAL, "newer");
    crate::services::tui_prompt_dedupe::release_herdr_execution(LOGICAL, &fx.nonce);
    assert_eq!(herdr_listed_execution(LOGICAL).as_deref(), Some("newer"));
}

// A draft, a held prompt, an unobserved Herdr Claude or one outliving its exit leaves the row
// Bound, the listing and the Herdr marker as they were, and stops the turn.
#[test]
fn b3_a_herdr_claude_not_ended_by_its_exit_keeps_its_row_and_host_pg() {
    let fx = Fixture::new();
    type Setup = fn(&Ports, &Fixture);
    let cases: [(&str, Setup, &str, usize); 4] = [
        (
            "draft",
            |p, _| p.herdr.lock().unwrap().draft = true,
            "kept running",
            0,
        ),
        (
            "hold",
            |_, fx| {
                let hold = crate::services::claude::herdr_turn::hold_path(&fx.nonce).unwrap();
                std::fs::create_dir_all(hold.parent().unwrap()).unwrap();
                std::fs::write(hold, "t").unwrap();
            },
            "unsubmitted prompt",
            0,
        ),
        (
            "unobserved",
            |p, _| p.herdr.lock().unwrap().unknown = true,
            "unobserved",
            0,
        ),
        (
            "stays",
            |p, _| p.herdr.lock().unwrap().stays = true,
            "did not exit",
            1,
        ),
    ];
    for (name, setup, says, exits) in cases {
        let ports = running();
        setup(&ports, &fx);
        let error = fx.run(&ports).unwrap_err();
        assert!(error.contains(says), "{name}: {error}");
        assert_eq!(ports.herdr.lock().unwrap().exits, exits, "{name}");
        assert_eq!(fx.state(), HostedState::Bound, "{name}");
        assert!(on_herdr(fx.marker()), "{name}");
        assert_eq!(
            herdr_listed_execution(LOGICAL),
            Some(fx.nonce.clone()),
            "{name}"
        );
        let _ = crate::services::claude::herdr_turn::unlink_hold(&fx.nonce);
    }
}

// A fresh move to Herdr left the channel's tmux Claude running: the return ends it by one
// `/exit`, then the Herdr Claude, and the tmux turn resumes the Herdr session under its account.
#[test]
fn b9_a_return_after_a_fresh_move_ends_the_running_tmux_then_herdr_and_resumes_the_session_pg() {
    let fx = Fixture::new();
    tc::write_claude_session_auth(LOGICAL, &fx.session, "work").unwrap();
    let ports = running();
    ports.tmux.lock().unwrap().alive = true;
    assert_eq!(fx.run(&ports), Ok(()));
    let tmux = ports.tmux.lock().unwrap();
    assert_eq!((tmux.exits, tmux.alive), (1, false));
    drop(tmux);
    assert_eq!(ports.herdr.lock().unwrap().exits, 1);
    assert_eq!(fx.state(), HostedState::Retired);
    assert!(!on_herdr(fx.marker()));
    assert_eq!(herdr_listed_execution(LOGICAL), None);
    let profile = tc::session_temp_path(LOGICAL, "auth_profile");
    assert_eq!(std::fs::read_to_string(profile).unwrap(), "work");
}

// A tmux Claude a draft keeps, one outliving its `/exit` or an unobserved one stops the return
// before Herdr is touched: no Herdr `/exit`, the row Bound, the listing and marker as they were.
#[test]
fn b10_a_running_tmux_not_ended_by_its_exit_leaves_herdr_untouched_pg() {
    let fx = Fixture::new();
    type Setup = fn(&mut Side);
    let cases: [(&str, Setup, &str); 3] = [
        ("draft", |tmux| tmux.draft = true, "kept running"),
        ("stays", |tmux| tmux.stays = true, "did not exit"),
        ("unobserved", |tmux| tmux.unknown = true, "unobserved"),
    ];
    for (name, setup, says) in cases {
        let ports = running();
        ports.tmux.lock().unwrap().alive = true;
        setup(&mut ports.tmux.lock().unwrap());
        let error = fx.run(&ports).unwrap_err();
        assert!(error.contains(says), "{name}: {error}");
        let herdr = ports.herdr.lock().unwrap();
        assert_eq!((herdr.exits, herdr.reads), (0, 0), "{name}");
        drop(herdr);
        assert_eq!(fx.state(), HostedState::Bound, "{name}");
        assert!(on_herdr(fx.marker()), "{name}");
        let listed = herdr_listed_execution(LOGICAL);
        assert_eq!(listed, Some(fx.nonce.clone()), "{name}");
        let profile = tc::session_temp_path(LOGICAL, "auth_profile");
        assert!(!std::path::Path::new(&profile).exists(), "{name}");
    }
}

// A Herdr Claude that already exited is not typed to; a return that stopped after its retire
// finishes on the next turn without reading Herdr again.
#[test]
fn b4_a_return_resumed_after_its_exit_or_its_retire_finishes_without_another_exit_pg() {
    let fx = Fixture::new();
    let ended = Ports::default();
    assert_eq!(fx.run(&ended), Ok(()));
    assert_eq!(ended.herdr.lock().unwrap().exits, 0);
    assert_eq!(fx.state(), HostedState::Retired);

    std::fs::write(tc::session_temp_path(LOGICAL, "host_kind"), "herdr").unwrap();
    install_herdr_execution(LOGICAL, &fx.nonce);
    assert!(!crate::services::turn_host::returned(LOGICAL, &fx.nonce));
    let unread = Ports::default();
    unread.herdr.lock().unwrap().unknown = true;
    assert_eq!(fx.run(&unread), Ok(()));
    assert_eq!(unread.herdr.lock().unwrap().reads, 0);
    assert!(!on_herdr(fx.marker()));
    assert_eq!(herdr_listed_execution(LOGICAL), None);
}

// A resume needs O to read the transcript and a launch of that session to have recorded its
// account; without either, or with an unreadable transcript, it stops before a running tmux ends.
#[test]
fn b5_b6_a_resume_without_cursor_account_proof_or_readable_transcript_ends_nothing_pg() {
    let fx = Fixture::new();
    let refused = |name: &str, says: &str| {
        let ports = running();
        ports.tmux.lock().unwrap().alive = true;
        let error = fx.run(&ports).unwrap_err();
        assert!(error.contains(says), "{name}: {error}");
        assert_eq!(ports.herdr.lock().unwrap().exits, 0, "{name}");
        assert_eq!(ports.tmux.lock().unwrap().exits, 0, "{name}");
        assert_eq!(fx.state(), HostedState::Bound, "{name}");
    };
    let path = fx.transcript(false);
    refused("no cursor", "O reads no cursor");
    fx.transcript(true);
    let record = tc::session_temp_path(LOGICAL, "auth_session");
    std::fs::remove_file(&record).unwrap();
    refused("no record", "recorded its account");
    tc::write_claude_session_auth(LOGICAL, &uuid::Uuid::new_v4().to_string(), "default").unwrap();
    refused("a record of another session", "recorded its account");
    tc::write_claude_session_auth(LOGICAL, &fx.session, "default").unwrap();
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().unwrap().to_path_buf();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let ports = running();
    let unreadable = fx.run(&ports);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(unreadable.is_err(), "{unreadable:?}");
    assert_eq!(ports.herdr.lock().unwrap().exits, 0);
    assert_eq!(fx.run(&running()), Ok(()));
}

// A Herdr Claude whose launch session moved on twice returns the last one under the launch's
// account; a move from a session the record does not name vouches for nothing.
#[test]
fn b7_a_session_moved_on_from_its_launch_returns_under_the_launch_account_pg() {
    let fx = Fixture::new();
    let (launched, cleared) = (
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    );
    tc::write_claude_session_auth(LOGICAL, &launched, "work").unwrap();
    tc::advance_claude_session_auth(LOGICAL, &launched, &cleared).unwrap();
    // The launch session no longer names the record once the pane moved off it.
    tc::advance_claude_session_auth(LOGICAL, &launched, &fx.session).unwrap();
    let error = fx.run(&running()).unwrap_err();
    assert!(error.contains("recorded its account"), "{error}");
    tc::advance_claude_session_auth(LOGICAL, &cleared, &fx.session).unwrap();
    let ports = running();
    assert_eq!(fx.run(&ports), Ok(()));
    let profile = tc::session_temp_path(LOGICAL, "auth_profile");
    assert_eq!(std::fs::read_to_string(profile).unwrap(), "work");
    assert_eq!(ports.herdr.lock().unwrap().exits, 1);
}

/// Makes `profile` the configured Claude account, with its own managed home under a scratch HOME.
pub(crate) fn claude_account(profile: &str) -> (TestEnvVarGuard, tempfile::TempDir, PathBuf) {
    let user = tempfile::tempdir().unwrap();
    let guard = TestEnvVarGuard::set_path_after_shared_test_env_lock("HOME", user.path());
    let claude = crate::services::provider::ProviderKind::Claude;
    let home =
        crate::services::provider_auth_profile::extra_account_home(&claude, profile).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let org = serde_yaml::to_string(&serde_json::json!({"version": 1, "agents": {},
        "provider_auth_profiles": {profile: {"provider": "claude", "home": home}},
        "provider_auth_primary_profiles": {"claude": profile}}))
    .unwrap();
    let path = crate::runtime_layout::org_schema_path(&crate::config::runtime_root().unwrap());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, org).unwrap();
    (guard, user, home)
}

// A named account's session is looked for in that account's own home, as its tmux turn does:
// found there it returns under that account; in the daemon's home only it starts anew.
#[test]
fn b8_a_named_account_session_returns_from_its_own_home_pg() {
    for own in [false, true] {
        let fx = Fixture::new();
        let (_account, _user, home) = claude_account("work");
        tc::write_claude_session_auth(LOGICAL, &fx.session, "work").unwrap();
        if own {
            fx.transcript_under(Some(&home), true);
        }
        let ports = running();
        assert_eq!(fx.run(&ports), Ok(()), "own home: {own}");
        let marker = std::fs::read_to_string(tc::session_temp_path(LOGICAL, "auth_profile"));
        assert_eq!(
            marker.ok().as_deref(),
            own.then_some("work"),
            "own home: {own}"
        );
        assert_eq!(ports.herdr.lock().unwrap().exits, 1);
    }
}
