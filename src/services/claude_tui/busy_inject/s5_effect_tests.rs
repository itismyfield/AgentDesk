use super::*;
use crate::test_support::input_owner::{Op, Scope, fixture};

#[test]
fn s5_b0_pinned_stash_applied_counts_the_actual_payload_once() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    let rig = Rig::new("s5-pinned-applied");
    let (_root, attempt, guard) = fixture("tmux", &rig.name);
    let _scope = guard.scope(Scope::owner(attempt.clone()));
    let (outcome, proof, starts) = rig.run(&FAST, false, usize::MAX, false, |_| {}, || {});
    assert_eq!((outcome, proof, starts), (Outcome::Injected, None, 1));
    guard.assert_owner_payload(&attempt, TEXT.as_bytes(), 1);
    assert!(rig.pinned_writes());
    let counts = guard.counts();
    assert_eq!(
        counts
            .events
            .iter()
            .filter(|event| event.op == Op::Paste && event.written)
            .count(),
        1
    );
    assert_eq!(
        counts
            .events
            .iter()
            .filter(|event| event.op == Op::Enter && event.written)
            .count(),
        1
    );
    wait("s5-pinned-enter", || {
        events(&rig.renderer).iter().any(|event| event == "Enter")
    });
    assert_eq!(fs::read_to_string(rig.renderer.join("body")).unwrap(), TEXT);
}

#[test]
fn s5_b0_pinned_stash_held_does_not_count_a_successful_shell_exit() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    let rig = Rig::new("s5-pinned-held");
    let (_root, attempt, guard) = fixture("tmux", &rig.name);
    let swap = rig.swap();
    let _scope = guard.scope(Scope::owner(attempt.clone()));
    let (outcome, proof, _) = rig.run(
        &FAST,
        false,
        usize::MAX,
        false,
        |_| {},
        move || swap.replace(),
    );
    assert_eq!(
        (outcome, proof),
        (Outcome::Failed(WriteFailure::Paste), None)
    );
    assert!(rig.pinned_writes());
    let counts = guard.counts();
    assert_eq!(counts.attempted_legacy, 0);
    assert!(counts.owner_content.is_empty());
    assert!(
        counts
            .events
            .iter()
            .any(|event| event.op == Op::Paste && event.accepted && !event.written)
    );
    assert!(rig.events().is_empty());
}

fn pane_aliases(rig: &Rig) -> Vec<String> {
    rig.f
        .tmux(&[
            "display-message",
            "-p",
            "-t",
            &rig.pane_id,
            "#{pane_id}\n#{window_id}\n#{session_id}\n#{session_name}\n=#{session_name}:\n#{session_name}:#{window_index}.#{pane_index}\n#{session_name}:#{window_name}.#{pane_index}",
        ])
        .lines()
        .map(String::from)
        .collect()
}

fn alias_args<'a>(op: &str, alias: &'a str, shell: &'a str) -> Vec<&'a str> {
    match op {
        "if-shell" => vec!["if-shell", "-F", "-t", alias, "1", shell, ""],
        "send-keys" => vec!["send-keys", "-t", alias, "-l", "--", "s5-alias-late"],
        "paste-buffer" => vec!["paste-buffer", "-b", "s5-alias-buffer", "-t", alias],
        _ => unreachable!(),
    }
}

fn late_alias_is_blocked(op: &str) {
    let _env = crate::config::TestRuntimeRootGuard::new();
    let rig = Rig::new(&format!("s5-alias-{op}"));
    let (_root, attempt, guard) = fixture("tmux", &rig.name);
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        assert_eq!(
            rig.run(&FAST, false, usize::MAX, false, |_| {}, || {}),
            (Outcome::Injected, None, 1)
        );
    }
    guard.assert_owner_payload(&attempt, TEXT.as_bytes(), 1);
    rig.f
        .tmux(&["set-buffer", "-b", "s5-alias-buffer", "s5-alias-late"]);
    let shell = format!("send-keys -t {} -l -- s5-alias-late", rig.pane_id);
    let prefix = format!("-u {op} ");
    let writes = || {
        fs::read_to_string(&rig.trace)
            .unwrap()
            .lines()
            .filter(|line| line.starts_with(&prefix))
            .count()
    };
    let original_writes = writes();
    for (index, alias) in pane_aliases(&rig).iter().enumerate() {
        let result = rig.pane.ok(&alias_args(op, alias, &shell));
        assert!(
            result.is_none(),
            "late alias {op} must be rejected before subprocess execution: {alias}; result={result:?}; extra_writes={}",
            writes() - original_writes
        );
        assert_eq!(writes(), original_writes, "no extra write may reach tmux");
        let counts = guard.counts();
        assert_eq!(counts.attempted_legacy, index + 1);
        assert_eq!(counts.owner_content.get(&attempt), Some(&1));
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                guard.assert_owner_payload(&attempt, TEXT.as_bytes(), 1);
            }))
            .is_err(),
            "an alias fallback must invalidate the owner success assertion"
        );
    }
}

#[test]
fn s5_b0_pinned_alias_if_shell_cannot_hide_a_late_write() {
    late_alias_is_blocked("if-shell");
}

#[test]
fn s5_b0_pane_alias_send_keys_cannot_hide_a_late_write() {
    late_alias_is_blocked("send-keys");
}

#[test]
fn s5_b0_window_alias_paste_cannot_hide_a_late_write() {
    late_alias_is_blocked("paste-buffer");
}

#[test]
fn s5_b0_alias_owner_writes_count_and_unrelated_panes_remain_independent() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    let rig = Rig::new("s5-alias-owner");
    let (_root, attempt, guard) = fixture("tmux", &rig.name);
    let aliases = pane_aliases(&rig);
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        for alias in &aliases {
            assert!(
                rig.pane
                    .ok(&["send-keys", "-t", alias, "-l", "--", TEXT])
                    .is_some()
            );
        }
    }
    guard.assert_owner_payload(&attempt, TEXT.as_bytes(), aliases.len());
    let other = rig.f.session("s5-alias-unrelated");
    assert_ne!(other, rig.pane_id);
    for alias in [other.as_str(), "s5-alias-unrelated:0.0"] {
        assert!(
            rig.pane
                .ok(&["send-keys", "-t", alias, "-l", "--", "unrelated"])
                .is_some()
        );
    }
    assert_eq!(guard.counts().attempted_legacy, 0);
    guard.assert_owner_payload(&attempt, TEXT.as_bytes(), aliases.len());
}

#[test]
fn s5_b0_raw_alias_preserves_explicit_server_options() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    let rig = Rig::new("s5-alias-raw");
    let (_root, attempt, guard) = fixture("tmux", &rig.name);
    let mut command = std::process::Command::new(&rig.f.real);
    command.args([
        "-L",
        &rig.f.socket,
        "-f",
        "/dev/null",
        "send-keys",
        &format!("-t{}", rig.pane_id),
        TEXT,
        "Enter",
    ]);
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        let flight = crate::test_support::input_owner::tmux(&command).unwrap();
        assert!(command.output().unwrap().status.success());
        flight.written();
    }
    guard.assert_owner_payload(&attempt, TEXT.as_bytes(), 1);
    assert!(crate::test_support::input_owner::tmux(&command).is_err());
    assert_eq!(guard.counts().attempted_legacy, 1);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            guard.assert_owner_payload(&attempt, TEXT.as_bytes(), 1);
        }))
        .is_err()
    );
}

#[test]
fn s5_b0_unresolved_alias_invalidates_owner_success() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    let rig = Rig::new("s5-alias-unresolved");
    let (_root, attempt, guard) = fixture("tmux", &rig.name);
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        assert!(
            rig.pane
                .ok(&["send-keys", "-t", &rig.pane_id, "-l", "--", TEXT])
                .is_some()
        );
    }
    guard.assert_owner_payload(&attempt, TEXT.as_bytes(), 1);
    let trace = fs::read_to_string(&rig.trace).unwrap();
    assert!(
        rig.pane
            .ok(&["send-keys", "-t%123456", "-l", "--", "late"])
            .is_none()
    );
    assert_eq!(guard.counts().attempted_legacy, 1);
    assert!(!fs::read_to_string(&rig.trace).unwrap()[trace.len()..].contains("send-keys"));
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            guard.assert_owner_payload(&attempt, TEXT.as_bytes(), 1);
        }))
        .is_err()
    );
}
