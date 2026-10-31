//! Engine epochs: main, tick and both hot-reload Contexts read, then the engines drop; every
//! reader's connection and thread go with them, so repeated epochs leave nothing behind.

use std::time::{Duration, Instant};

use futures::FutureExt;

use super::contract_tests::{seed, until, write};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::health_diagnostics::pg_wire_proxy::PgWireProxy;

const EPOCHS: usize = 4;
/// Main, tick and their two reload Contexts.
const READING_CONTEXTS: usize = 4;
/// Runtime and watcher threads that may still start lazily after the first epoch.
const THREAD_SLACK: usize = 4;
const RERUN_ENV: &str = "POLICY_READ_BRIDGE_EPOCH_RERUN";
const RERUN_LIMIT: Duration = Duration::from_secs(180);

/// The policy reads at load time and in its hook, and names itself after what it read.
fn policy_source(epoch: usize) -> String {
    format!(
        r#"// epoch {epoch}
var tag = agentdesk.config.get("amp_scalar") + "-" + agentdesk.kv.get("live");
agentdesk.registerPolicy({{
    name: "amp-" + tag,
    onAmpRead: function() {{
        globalThis.__ampRead = agentdesk.config.get("amp_scalar") + "-" + agentdesk.kv.get("live");
    }}
}});
"#
    )
}

#[cfg(target_os = "linux")]
fn process_threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .expect("read /proc/self/task")
        .count()
}

/// `ps -M` prints a header and one line per thread of the process.
#[cfg(not(target_os = "linux"))]
fn process_threads() -> usize {
    let output = std::process::Command::new("ps")
        .args(["-M", "-p", &std::process::id().to_string()])
        .output()
        .expect("run ps -M");
    assert!(output.status.success(), "ps -M failed");
    String::from_utf8_lossy(&output.stdout).lines().count() - 1
}

/// Thread counts need the test alone: outside the rerun this runs it alone in a child of this
/// binary and requires that rerun to pass; inside the rerun it returns false.
fn rerun_alone(test: &str) -> bool {
    if std::env::var_os(RERUN_ENV).is_some() {
        return false;
    }
    let name = format!("{}::{test}", module_path!().split_once("::").unwrap().1);
    let log = tempfile::NamedTempFile::new().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture"])
        .env(RERUN_ENV, "1")
        .stdout(log.reopen().unwrap())
        .stderr(log.reopen().unwrap())
        .spawn()
        .expect("rerun the test binary");
    let deadline = Instant::now() + RERUN_LIMIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let output = std::fs::read_to_string(log.path()).unwrap();
    assert!(
        status.is_some_and(|status| status.success())
            && output.contains("test result: ok. 1 passed"),
        "the rerun of {name} failed ({status:?}):\n{output}"
    );
    true
}

fn names(engine: &crate::engine::PolicyEngine) -> Vec<String> {
    engine
        .list_policies()
        .into_iter()
        .map(|policy| policy.name)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_read_bridge_returns_connections_and_threads_across_engine_epochs() {
    if rerun_alone("policy_read_bridge_returns_connections_and_threads_across_engine_epochs") {
        return;
    }
    let db = TestPostgresDb::create().await;
    let direct = db.connect_and_migrate().await;
    seed(&direct, "s1", "start");
    let proxy = PgWireProxy::start(&db.database_url, Duration::ZERO).await;
    let pool = proxy.pool(2);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("amp-read.js");
    let mut config = crate::config::Config::default();
    config.policies.dir = dir.path().to_path_buf();
    config.policies.hot_reload = true;
    let mut first_threads = None;

    for epoch in 0..EPOCHS {
        write(
            &direct,
            &[format!("agentdesk.kv.set('live', 'load{epoch}', 0)")],
        );
        std::fs::write(&file, policy_source(epoch)).unwrap();
        let accepted = proxy.accepted();
        let main = crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone())).unwrap();
        let tick = crate::engine::PolicyEngine::new_for_tick(&config, Some(pool.clone())).unwrap();
        let loaded = vec![format!("amp-s1-load{epoch}")];
        assert_eq!((names(&main), names(&tick)), (loaded.clone(), loaded));
        for engine in [&main, &tick] {
            engine
                .try_fire_hook_by_name("onAmpRead", serde_json::json!({}))
                .unwrap();
            assert_eq!(
                engine.eval_js::<String>("globalThis.__ampRead").unwrap(),
                format!("s1-load{epoch}")
            );
        }

        // A changed file makes each engine's reload Context read again.
        write(
            &direct,
            &[format!("agentdesk.kv.set('live', 'reload{epoch}', 0)")],
        );
        std::fs::write(&file, policy_source(epoch) + "// changed\n").unwrap();
        let reloaded = vec![format!("amp-s1-reload{epoch}")];
        let waited = std::panic::AssertUnwindSafe(until(
            "both reload Contexts to read the new value",
            || names(&main) == reloaded && names(&tick) == reloaded,
        ))
        .catch_unwind()
        .await;
        if let Err(panic) = waited {
            eprintln!(
                "epoch {epoch}: expected {reloaded:?}, main={:?}, tick={:?}, accepted={}, open={}, SQL={:?}",
                names(&main),
                names(&tick),
                proxy.accepted(),
                proxy.open(),
                proxy.executions()
            );
            std::panic::resume_unwind(panic);
        }
        let opened = proxy.accepted() - accepted;
        assert_eq!(
            opened, READING_CONTEXTS,
            "epoch {epoch}: one connection per reading Context"
        );

        drop((main, tick));
        until("every reader of the dropped engines to close", || {
            proxy.open() == 0
        })
        .await;
        let threads = process_threads();
        let first = *first_threads.get_or_insert(threads);
        assert!(
            threads <= first + THREAD_SLACK,
            "epoch {epoch}: threads {first} -> {threads}"
        );
    }

    pool.close().await;
    direct.close().await;
    drop(proxy);
    db.drop().await;
}
