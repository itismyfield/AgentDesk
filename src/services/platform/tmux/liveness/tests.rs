use super::*;
use std::cell::RefCell;

pub(crate) fn output(code: i32, stdout: &str, stderr: &str) -> Result<Output, String> {
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    Ok(Output {
        #[cfg(unix)]
        status: std::process::ExitStatus::from_raw(code << 8),
        #[cfg(windows)]
        status: std::process::ExitStatus::from_raw(code as u32),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    })
}

pub(crate) fn probe(
    name: &str,
    budget: Duration,
    replies: Vec<Result<Output, String>>,
) -> PaneLiveness {
    let mut replies = replies.into_iter();
    let result = pane_liveness_within_using(
        name,
        budget,
        || Some(Command::new("unused-fake-tmux")),
        |command, timeout, _| {
            assert!(timeout > Duration::ZERO && timeout <= budget.min(Duration::from_secs(2)));
            let args: Vec<_> = command
                .get_args()
                .map(|arg| arg.to_string_lossy())
                .collect();
            assert_eq!(args[1..3], ["-t", &format!("={name}:")]);
            replies.next().expect("unexpected additional probe")
        },
    );
    assert!(
        replies.next().is_none(),
        "probe stopped before expected command"
    );
    result
}

#[test]
fn bounded_probe_shares_budget_and_stops_before_expired_spawn() {
    let budgets = RefCell::new(Vec::new());
    let result = pane_liveness_within_using(
        "test-pane",
        Duration::from_secs(1),
        || Some(Command::new("unused-fake-tmux")),
        |_, budget, _| {
            budgets.borrow_mut().push(budget);
            std::thread::sleep(Duration::from_millis(20));
            output(0, "0\n", "")
        },
    );
    assert_eq!(result, PaneLiveness::Live);
    let budgets = budgets.into_inner();
    assert_eq!(budgets.len(), 2);
    assert!(budgets[1] + Duration::from_millis(15) < budgets[0]);

    for (budget, prepare_delay, run_delay, ready, expected_calls) in [
        (Duration::ZERO, 0, 0, true, 0),
        (Duration::from_millis(10), 30, 0, true, 0),
        (Duration::from_millis(10), 0, 30, true, 1),
        (Duration::from_secs(1), 0, 0, false, 0),
    ] {
        let mut calls = 0;
        let result = pane_liveness_within_using(
            "test-pane",
            budget,
            || {
                std::thread::sleep(Duration::from_millis(prepare_delay));
                ready.then(|| Command::new("unused-fake-tmux"))
            },
            |_, _, _| {
                calls += 1;
                std::thread::sleep(Duration::from_millis(run_delay));
                output(0, "0\n", "")
            },
        );
        assert_eq!(result, PaneLiveness::ProbeError);
        assert_eq!(calls, expected_calls);
    }
    assert_eq!(
        probe(
            "test-pane",
            Duration::from_secs(9),
            vec![output(0, "", ""), output(0, "0\n", "")]
        ),
        PaneLiveness::Live
    );
}

#[test]
fn legacy_probe_keeps_two_seconds_for_each_command() {
    let mut calls = 0;
    assert_eq!(
        pane_liveness_using(
            "test-pane",
            || Some(Command::new("unused-fake-tmux")),
            |_, budget, _| {
                calls += 1;
                assert_eq!(budget, Duration::from_secs(2));
                output(0, "0\n", "")
            },
        ),
        PaneLiveness::Live
    );
    assert_eq!(calls, 2);
    assert_eq!(
        pane_liveness_using("test-pane", || None, |_, _, _| unreachable!()),
        PaneLiveness::ProbeError
    );
}
