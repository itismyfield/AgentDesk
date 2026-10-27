use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::runtime_store::durable_publish::flush_probe::{Plan, Step, Watch};
use crate::services::tui_input::human_frame::idle;

fn read(history: &Registry) -> String {
    std::fs::read_to_string(&history.path).unwrap()
}

fn watch(history: &Registry, fail: Vec<Step>) -> Watch {
    Watch::new(history.path.parent().unwrap(), Plan { fail, block: None })
}

fn events(history: &Registry) -> Vec<Value> {
    history.json().unwrap()["passive"]
        .as_array()
        .unwrap()
        .iter()
        .chain(history.json().unwrap()["live"].as_array().unwrap())
        .flat_map(|p| {
            p.get("entry")
                .unwrap_or(p)
                .get("events")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .collect()
}

fn not_sent(input: &Value, phase: &str) -> Value {
    let mut input = input.clone();
    input["phase"] = json!(phase);
    json!({"input": input, "disposition": "known_not_sent"})
}

#[test]
fn s4_hist_local_submits_stay_native_candidates_in_both_query_orders() {
    for (reverse, pre_source) in [(false, false), (true, false), (false, true), (true, true)] {
        let _root = TestRuntimeRootGuard::new();
        let (_dir, path, history) = setup();
        let frame = idle("ann", "7", "same local submit");
        let a = key(500, &path, pre_source);
        let b = key(501, &path, false);
        history.prepare("history-test", &input(&a, &frame)).unwrap();
        history.prepare("history-test", &input(&b, &frame)).unwrap();
        history
            .project(&projection(None, &[], &[a.clone(), b.clone()]))
            .unwrap();
        let state = history.json().unwrap();
        assert_eq!(state["live"].as_array().unwrap().len(), 0);
        assert_eq!(
            state["planned"].as_array().unwrap().len(),
            0,
            "local submit returns every slot"
        );
        let order = if reverse { [&b, &a] } else { [&a, &b] };
        for key in order {
            assert_eq!(
                history.receipt(
                    "history-test",
                    key,
                    (&path, 0, "codex", Some("late-row"), &frame)
                ),
                None,
                "one late row cannot be given to either local submit, pre_source={pre_source}"
            );
        }
        assert!(events(&history).is_empty(), "no attempt owns the late row");
        assert!(super::super::super::known(&frame));
    }
}

#[test]
fn s4_hist_an_identified_row_keeps_only_its_exact_owner() {
    let _root = TestRuntimeRootGuard::new();
    let (_dir, path, history) = setup();
    let frame = idle("ann", "7", "identified");
    let a = key(510, &path, false);
    let b = key(511, &path, false);
    history.prepare("history-test", &input(&a, &frame)).unwrap();
    let row = (&*path, 0, "codex", Some("row-A"), frame.as_str());
    assert_eq!(history.receipt("history-test", &a, row), Some(true));
    history.prepare("history-test", &input(&b, &frame)).unwrap();
    history
        .project(&projection(None, &[], &[a.clone(), b.clone()]))
        .unwrap();
    assert_eq!(history.receipt("history-test", &b, row), None);
    assert_eq!(history.receipt("history-test", &a, row), Some(true));
    assert_eq!(events(&history).len(), 1);
}

#[test]
fn history_v2_only_an_exact_not_sent_reserved_attempt_releases_its_candidate() {
    let _root = TestRuntimeRootGuard::new();
    let (_dir, path, history) = setup();
    let frame = idle("ann", "7", "abandon me");
    let a = key(520, &path, false);
    let b = key(521, &path, false);
    let ia = input(&a, &frame);
    history.prepare("history-test", &ia).unwrap();
    history.prepare("history-test", &input(&b, &frame)).unwrap();
    let row = (&*path, 0, "codex", Some("row-B"), frame.as_str());
    for unproven in [json!({"input": ia}), not_sent(&ia, "may_have_sent")] {
        let mut snapshot = projection(None, &[], std::slice::from_ref(&b));
        snapshot["retained_inputs"] = json!([unproven]);
        history.project(&snapshot).unwrap();
        assert_eq!(
            history.receipt("history-test", &b, row),
            None,
            "a retained attempt without the exact Reserved proof may still own the row"
        );
    }
    let mut snapshot = projection(None, &[], std::slice::from_ref(&b));
    snapshot["retained_inputs"] = json!([not_sent(&ia, "reserved")]);
    history.project(&snapshot).unwrap();
    assert_eq!(history.receipt("history-test", &b, row), Some(true));
    let passive = history.json().unwrap()["passive"].clone();
    assert!(
        passive
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["state"] == "abandoned" && p["registration"]["key"] == a)
    );

    let lone = idle("ann", "7", "never pasted");
    let c = key(522, &path, false);
    let ic = input(&c, &lone);
    history.prepare("history-test", &ic).unwrap();
    assert!(super::super::super::known(&lone));
    let mut snapshot = projection(None, &[], &[]);
    snapshot["retained_inputs"] = json!([not_sent(&ic, "reserved")]);
    history.project(&snapshot).unwrap();
    assert!(
        !super::super::super::known(&lone),
        "a frame proven never pasted marks no native row as injected"
    );
}

#[test]
fn history_v2_scoped_withdraw_and_unchanged_projection_publish_nothing() {
    let _root = TestRuntimeRootGuard::new();
    let (_dir, path, history) = setup();
    let frame = idle("ann", "7", "withdrawn");
    let a = key(530, &path, false);
    history.prepare("history-test", &input(&a, &frame)).unwrap();
    let snapshot = projection(None, &[], std::slice::from_ref(&a));
    history.project(&snapshot).unwrap();
    let before = read(&history);
    let probe = watch(&history, vec![]);
    with_attempt(&history, "history-test", &a, || {
        super::super::super::withdraw("nonce530", &path)
    })
    .unwrap();
    history.project(&snapshot).unwrap();
    assert!(
        probe.events().is_empty(),
        "no-op transactions write or sync nothing"
    );
    assert_eq!(read(&history), before);
    assert!(
        super::super::super::known(&frame),
        "withdraw keeps the candidate"
    );
}

#[test]
fn history_v2_unconfirmed_publication_recovers_or_refuses_a_third_state() {
    for third in [false, true] {
        let _root = TestRuntimeRootGuard::new();
        let (_dir, path, history) = setup();
        let a = key(540, &path, false);
        let b = key(541, &path, false);
        {
            let _failing = watch(&history, vec![Step::ParentSync]);
            assert!(history.prepare("history-test", &input(&a, "A")).is_err());
            let candidate = read(&history);
            assert!(
                history.prepare("history-test", &input(&b, "B")).is_err(),
                "an undurable candidate is never built on"
            );
            assert_eq!(read(&history), candidate);
        }
        if third {
            let other = "{\n  \"entries\": []\n}";
            std::fs::write(&history.path, other).unwrap();
            assert!(history.prepare("history-test", &input(&b, "B")).is_err());
            assert_eq!(read(&history), other, "third state preserved");
            continue;
        }
        let probe = watch(&history, vec![]);
        history.prepare("history-test", &input(&b, "B")).unwrap();
        assert!(!probe.flushed(Step::ParentSync).is_empty());
        assert_eq!(history.json().unwrap()["live"].as_array().unwrap().len(), 2);
    }
}

#[test]
fn history_v2_encoding_failure_discards_the_unpublished_state() {
    let _root = TestRuntimeRootGuard::new();
    let (_dir, path, history) = setup();
    let a = key(550, &path, false);
    let b = key(551, &path, false);
    history.prepare("history-test", &input(&a, "A")).unwrap();
    history
        .encode_fault
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(history.prepare("history-test", &input(&b, "B")).is_err());
    history
        .encode_fault
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        history.json().unwrap()["live"].as_array().unwrap().len(),
        1,
        "an unpublished registration never survives in the cache"
    );
    assert!(history.source("history-test", &b).is_err());
}

#[cfg(unix)]
fn cpu_us() -> u128 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime only writes the timespec it is given.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) };
    t.tv_sec as u128 * 1_000_000 + t.tv_nsec as u128 / 1_000
}
#[cfg(not(unix))]
fn cpu_us() -> u128 {
    0
}

type Measured<'a> = (&'static str, Box<dyn Fn() -> bool + 'a>);

#[test]
fn history_v2_cost_is_one_publish_per_change_at_zero_1024_and_5001_rows() {
    for rows in [0usize, 1024, 5001] {
        let _root = TestRuntimeRootGuard::new();
        let (_dir, path, history) = setup();
        let seed = key(1, &path, false);
        let bound = source(&seed, &path);
        history
            .transaction(|state| {
                for i in 0..rows {
                    state.passive.push(Passive {
                        registration: None,
                        entry: Some(Frame::new("7", &format!("old {i}")).bind(&seed, &bound)),
                        state: "settled".into(),
                        encoded: None,
                    });
                }
                Ok(())
            })
            .unwrap();
        let frame = idle("ann", "7", "measured");
        let a = key(560, &path, false);
        let snapshot = projection(None, &[], std::slice::from_ref(&a));
        let steps: [Measured; 4] = [
            (
                "prepare",
                Box::new(|| history.prepare("history-test", &input(&a, &frame)).is_ok()),
            ),
            (
                "receipt",
                Box::new(|| {
                    history.receipt("history-test", &a, (&path, 0, "codex", Some("r"), &frame))
                        == Some(true)
                }),
            ),
            ("project", Box::new(|| history.project(&snapshot).is_ok())),
            ("noop", Box::new(|| history.project(&snapshot).is_ok())),
        ];
        for (name, step) in steps {
            let probe = watch(&history, vec![]);
            let (wall, cpu) = (std::time::Instant::now(), cpu_us());
            assert!(step(), "{name} at {rows} rows");
            let (cpu, wall) = (cpu_us() - cpu, wall.elapsed().as_micros());
            let writes = probe.count(Step::Write);
            let syncs = probe.count(Step::FileSync)
                + probe.count(Step::ParentSync)
                + probe.count(Step::Ancestors);
            println!(
                "HISTORY_U20 rows={rows} step={name} bytes={} publishes={writes} fsyncs={syncs} wall_us={wall} cpu_us={cpu}",
                std::fs::metadata(&history.path).unwrap().len()
            );
            assert_eq!(writes, usize::from(name != "noop"), "{name} at {rows} rows");
            assert_eq!(syncs == 0, name == "noop", "{name} at {rows} rows");
        }
    }
}
