//! Isolated real Herdr/OS/PG fixture shared with the marker-loss recovery tests.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{self, HostedObservation};
use crate::services::discord::SharedData;
use crate::services::discord::herdr_terminate::root_replaced::{self as recovery, CAS_ATTEMPTS};
use crate::services::session_host::herdr::model::HerdrRequest;
use crate::services::session_host::herdr::wire::LineJsonFraming;
use crate::services::session_host::herdr_terminate_real_tests as real;
use crate::services::session_host::{PaneProvider, PaneReading, herdr_endpoints};
use crate::services::tui_prompt_dedupe as dedupe;
pub(crate) const CHANNEL: u64 = 78_780_031;
pub(crate) const TOKEN: &str = "missing-marker-test";
pub(crate) const LOGICAL: &str = "AgentDesk-claude-missing-marker-test";

pub(crate) fn child(name: &str, module: &str) -> bool {
    if std::env::var("ADK_MM_CHILD").as_deref() == Ok(name) {
        return true;
    }
    let base = std::env::var_os("ADK_MM_ARTIFACTS")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("mm-{name}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let scratch = tempfile::Builder::new()
        .prefix("adk-mm-")
        .tempdir()
        .unwrap();
    let output = std::process::Command::new("bash")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/missing_marker_nextest.sh"
        ))
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!(
                "{}::{name}",
                module.strip_prefix("agentdesk::").unwrap_or(module)
            ),
            "--nocapture",
        ])
        .env("ADK_MM_CHILD", name)
        .env("ADK_TEST_HERDR_ROOT", scratch.path())
        .env("ADK_TEST_HERDR_PIDS", scratch.path().join("pids.log"))
        .output()
        .unwrap();
    copy_tree(scratch.path(), &dir);
    std::fs::write(
        dir.join("test.log"),
        [&output.stdout[..], &output.stderr[..]].concat(),
    )
    .unwrap();
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed; 0 ignored;"),
        "child must execute exactly one real scenario: {}\n{}\nartifacts={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        dir.display()
    );
    eprintln!("MM_REAL_OK {name} artifacts={}", dir.display());
    false
}

pub(crate) struct Case {
    pub(crate) real: real::Real,
    pub(crate) rt: tokio::runtime::Runtime,
    _db: TestPostgresDb,
    pub(crate) pool: PgPool,
    pub(crate) execution: real::Execution,
    pub(crate) shared: Arc<SharedData>,
    _root: crate::config::TestRuntimeRootGuard,
}

impl Case {
    pub(crate) fn native_binary(&self, path: &std::path::Path) -> crate::config::TestEnvVarGuard {
        crate::config::test_env_lock::assert_shared_test_env_lock_held();
        assert_eq!(
            crate::config::runtime_root().as_deref(),
            Some(self._root.path())
        );
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CLAUDE_PATH",
            path,
        )
    }

    pub(crate) fn new() -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let mut server = real::Real::start(&real::Scratch::from_env(), "missing-marker");
        std::os::unix::fs::symlink("/bin/bash", server.dir.join("pathbin/bash")).unwrap();
        let config: crate::config::Config = serde_json::from_value(json!({
            "server": {}, "cluster": {"instance_id": "mac-mini"},
            "agents": [{"id": "mm", "name": "mm", "provider": "claude", "channels": {
                "claude": {"id": CHANNEL.to_string(), "name": "missing-marker-test", "runtime": "tui"}}}],
            "session_hosts": {"herdr": {"endpoints": {"missing-marker": {
                "execution_node": "mac-mini", "socket_path": server.socket(),
                "herdr_home": server.socket().parent().unwrap().parent().unwrap().join("home"),
                "herdr_session": "adk"}}, "channels": {CHANNEL.to_string(): "missing-marker"}}}
        })).unwrap();
        crate::config::session_hosts::install(&config).unwrap();
        crate::config_live_reload::install(config);
        let binding_root = root.path().to_path_buf();
        let install = move || {
            crate::services::tui_prompt_dedupe::binding_events::set_test_root(Some(&binding_root));
            let registry =
                crate::config::session_hosts::with_boot(|boot| HerdrRegistry::build(boot.unwrap()));
            std::mem::forget(herdr_registry::force_for_test(registry));
        };
        install();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .on_thread_start(install)
            .build()
            .unwrap();
        let (db, pool) = rt.block_on(async {
            let db = TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            (db, pool)
        });
        let database: String = rt
            .block_on(sqlx::query_scalar("SELECT current_database()").fetch_one(&pool))
            .unwrap();
        eprintln!("MM_PG_REAL database={database}");
        let cwd = server.work("same-cwd");
        let execution = rt.block_on(real::launch(
            &mut server,
            &pool,
            TOKEN,
            CHANNEL,
            LOGICAL,
            cwd,
        ));
        let mut shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        Arc::get_mut(&mut shared).unwrap().token_hash = TOKEN.into();
        shared
            .restart
            .reconcile_done
            .store(true, std::sync::atomic::Ordering::Release);
        recovery::prune(&[]);
        CAS_ATTEMPTS.with(|count| count.set(0));
        Self {
            real: server,
            rt,
            _db: db,
            pool,
            execution,
            shared,
            _root: root,
        }
    }
    pub(crate) fn record(&self) -> HostedExecution {
        self.rt
            .block_on(real::stored(&self.pool, &self.execution.key))
    }
    pub(crate) fn observation(&self) -> HostedObservation {
        match self.rt.block_on(hosted_execution::load_hosted_execution_pg(
            &self.pool,
            HostedLookupKey::SessionKey(&self.execution.key),
        )) {
            HostedLookup::Found(row) => row,
            other => panic!("{other:?}"),
        }
    }
    pub(crate) fn store(&self, record: &HostedExecution) {
        self.rt.block_on(async {
            sqlx::query("UPDATE sessions SET hosted_execution=$1 WHERE session_key=$2")
                .bind(serde_json::to_value(record).unwrap())
                .bind(&self.execution.key)
                .execute(&self.pool)
                .await
                .unwrap();
        });
    }
    pub(crate) fn marker(&self) -> PathBuf {
        crate::services::tmux_common::session_temp_path(LOGICAL, "host_kind").into()
    }
    pub(crate) fn lose_marker(&self) {
        std::fs::remove_file(self.marker()).unwrap();
        crate::services::claude::herdr_turn::hold(&self.execution.nonce).unwrap();
        dedupe::release_herdr_execution(LOGICAL, &self.execution.nonce);
    }
    pub(crate) fn signal(&self, signal: i32) {
        assert!(real::alive(&self.execution.provider));
        assert_eq!(
            unsafe { libc::kill(self.execution.provider.0 as i32, signal) },
            0
        );
    }
    pub(crate) fn ended(&self) {
        self.signal(libc::SIGTERM);
        wait(
            || !real::alive(&self.execution.provider),
            "old provider exited",
        );
        self.shell();
    }
    pub(crate) fn shell(&self) {
        let record = self.record();
        wait(
            || {
                matches!(
                    herdr_endpoints().view(&record).unwrap().read_execution(),
                    PaneReading::Present {
                        provider: PaneProvider::Exited,
                        ..
                    }
                )
            },
            "lone shell",
        );
    }
    pub(crate) fn occupy_unverified(&self) {
        let endpoint = self.real.endpoint();
        let transport =
            HerdrSocketTransport::new(&endpoint, HerdrSocketConfig::default(), LineJsonFraming);
        let request = HerdrCall {
            id: "unrelated-command".into(),
            request: HerdrRequest::PaneSendInput {
                pane_id: self.execution.pane.clone(),
                text: "/bin/sleep 3600".into(),
                keys: vec!["enter".into()],
            },
        };
        assert!(
            transport
                .call_with_witness(&request, &transport.server_witness().unwrap())
                .unwrap()
                .body
                .is_ok()
        );
        let record = self.record();
        wait(
            || {
                matches!(
                    herdr_endpoints().view(&record).unwrap().read_execution(),
                    PaneReading::Present {
                        provider: PaneProvider::Unverified(_),
                        ..
                    }
                )
            },
            "unverified occupant",
        );
    }
    pub(crate) fn assert_kept(&self, row: &HostedObservation, hold: &[u8]) {
        assert_eq!(self.observation(), *row, "exact Bound row preserved");
        assert_eq!(CAS_ATTEMPTS.with(std::cell::Cell::get), 0);
        assert_eq!(
            std::fs::read(
                crate::services::claude::herdr_turn::hold_path(&self.execution.nonce).unwrap()
            )
            .unwrap(),
            hold
        );
    }
    pub(crate) fn hold_bytes(&self) -> Vec<u8> {
        std::fs::read(
            crate::services::claude::herdr_turn::hold_path(&self.execution.nonce).unwrap(),
        )
        .unwrap()
    }
}
impl Drop for Case {
    fn drop(&mut self) {
        // Failed assertions must not leave this fixture's stopped provider behind.
        if real::alive(&self.execution.provider) {
            unsafe {
                libc::kill(self.execution.provider.0 as i32, libc::SIGCONT);
            }
        }
        copy_tree(
            self._root.path(),
            &self.real.dir.parent().unwrap().join("runtime-proof"),
        );
        self.rt.block_on(async {
            self.pool.close().await;
        });
    }
}
pub(crate) fn wait(mut f: impl FnMut() -> bool, detail: &str) {
    let end = Instant::now() + Duration::from_secs(15);
    while !f() {
        assert!(Instant::now() < end, "{detail}");
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for item in std::fs::read_dir(from).unwrap() {
        let item = item.unwrap();
        let target = to.join(item.file_name());
        if item.file_type().unwrap().is_dir() {
            copy_tree(&item.path(), &target);
        } else if item.file_type().unwrap().is_file() {
            std::fs::copy(item.path(), target).unwrap();
        }
    }
}
