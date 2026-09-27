use super::*;
use crate::engine::loader::ScopedBridgeDeadline;
use crate::services::platform::tmux::liveness_tests::{output, probe};
use std::cell::Cell;
use std::time::Instant;

fn with_op(
    probe: impl Fn(String, Duration) -> HostLiveness + 'static,
    check: impl FnOnce(Ctx<'_>),
) {
    let runtime = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&runtime).unwrap();
    context.with(|ctx| {
        ctx.globals()
            .set("agentdesk", Object::new(ctx.clone()).unwrap())
            .unwrap();
        register_exec_ops(&ctx).unwrap();
        let ad: Object = ctx.globals().get("agentdesk").unwrap();
        let session: Object = ad.get("session").unwrap();
        register_session_liveness_op(&ctx, &session, probe).unwrap();
        check(ctx);
    });
}

#[test]
fn js_liveness_preserves_owner_live_dead_and_unknown() {
    let cases = [
        ("live", vec![output(0, "", ""), output(0, "0\n", "")]),
        ("live", vec![output(0, "", ""), output(0, "1\n0\n", "")]),
        ("dead", vec![output(1, "", "can't find session: test-pane")]),
        ("dead", vec![output(1, "", "no server running")]),
        ("dead", vec![output(0, "", ""), output(0, "1\n1\n", "")]),
        (
            "unknown",
            vec![Err(
                std::io::Error::from(std::io::ErrorKind::NotFound).to_string()
            )],
        ),
        (
            "unknown",
            vec![Err(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            )
            .to_string())],
        ),
        ("unknown", vec![output(1, "", "permission denied")]),
        ("unknown", vec![output(1, "", "unexpected failure")]),
        ("unknown", vec![Err("tmux has-session timed out".into())]),
        (
            "unknown",
            vec![output(0, "", ""), Err("tmux list-panes timed out".into())],
        ),
        (
            "unknown",
            vec![output(0, "", ""), output(1, "", "session disappeared")],
        ),
    ];
    for (expected, replies) in cases {
        let replies = std::cell::RefCell::new(Some(replies));
        with_op(
            move |name, budget| {
                assert_eq!(name, "test-pane");
                probe(&name, budget, replies.borrow_mut().take().unwrap()).into()
            },
            |ctx| {
                let actual: String = ctx
                    .eval("agentdesk.session.hasLivePane('test-pane')")
                    .unwrap();
                assert_eq!(actual, expected);
            },
        );
    }
}

#[test]
fn js_liveness_honors_bridge_budget_and_blank_input() {
    for limit in [None, Some(Duration::from_millis(200)), Some(Duration::ZERO)] {
        let _scope = limit.map(ScopedBridgeDeadline::new);
        let calls = std::rc::Rc::new(Cell::new(0));
        let probe_calls = calls.clone();
        with_op(
            move |name, budget| {
                probe_calls.set(probe_calls.get() + 1);
                assert!(
                    budget > Duration::ZERO && budget <= limit.unwrap_or(Duration::from_secs(2))
                );
                probe(&name, budget, vec![output(0, "", ""), output(0, "0\n", "")]).into()
            },
            |ctx| {
                let blank: String = ctx.eval("agentdesk.session.hasLivePane('  ')").unwrap();
                assert_eq!(blank, "unknown");
                assert_eq!(calls.get(), 0);
                let actual: String = ctx
                    .eval("agentdesk.session.hasLivePane('test-pane')")
                    .unwrap();
                assert_eq!(
                    actual,
                    if limit == Some(Duration::ZERO) {
                        "unknown"
                    } else {
                        "live"
                    }
                );
                assert_eq!(calls.get(), usize::from(limit != Some(Duration::ZERO)));
            },
        );
    }
}

#[test]
fn cold_snapshot_returns_unknown_without_discovery() {
    const CHILD: &str = "ADK_LIVENESS_COLD_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "engine::ops::exec_ops::session_liveness_tests::cold_snapshot_returns_unknown_without_discovery", "--nocapture"])
            .env(CHILD, "1").stdout(Stdio::piped()).stderr(Stdio::piped());
        configure_child_process_group(&mut command);
        let child = command.spawn().unwrap();
        let out =
            wait_with_output_timeout(child, Duration::from_secs(5), "cold snapshot test").unwrap();
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        print!("{}", String::from_utf8_lossy(&out.stdout));
        return;
    }
    let runtime = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&runtime).unwrap();
    context.with(|ctx| {
        ctx.globals()
            .set("agentdesk", Object::new(ctx.clone()).unwrap())
            .unwrap();
        register_exec_ops(&ctx).unwrap();
        assert!(crate::services::platform::binary_resolver::prepared_runtime_path().is_none());
        let start = Instant::now();
        let actual: String = ctx
            .eval("agentdesk.session.hasLivePane('test-cold-pane')")
            .unwrap();
        assert_eq!(actual, "unknown");
        assert!(start.elapsed() < Duration::from_secs(1));
        println!("cold unknown returned in {:?}", start.elapsed());
        assert!(crate::services::platform::binary_resolver::prepared_runtime_path().is_none());
    });
}
