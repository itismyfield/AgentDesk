use super::*;
use crate::services::claude_tui::host_input::InputTransport;
use crate::services::platform::tmux;
use crate::services::tui_input::submission_tmux::SubmissionTmux;

fn refuses_success(guard: &StrictOwnerEffectGuard, attempt: &Attempt, payload: &[u8]) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        guard.assert_owner_payload(attempt, payload, 1);
    }))
    .is_err()
}

#[test]
fn s5_b0_bounded_late_unscoped_write_is_refused_before_spawn() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    let (root, attempt, guard) = fixture("tmux", "s5-bounded-late");
    let _fake = crate::services::tui_prompt_dedupe::binding_context::tests::fake_tmux(root.path());
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        assert!(
            tmux::send_literal(&attempt.target.physical, "frame")
                .unwrap()
                .status
                .success()
        );
    }
    guard.assert_owner_payload(&attempt, b"frame", 1);
    assert!(
        SubmissionTmux
            .send_literal(&attempt.target.physical, "late fallback")
            .is_err(),
        "bounded fallback must be rejected before subprocess execution"
    );
    assert_eq!(guard.counts().attempted_legacy, 1);
    assert_eq!(guard.counts().owner_content.get(&attempt), Some(&1));
    assert!(refuses_success(&guard, &attempt, b"frame"));
    let runtime = std::path::PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    assert_eq!(
        std::fs::read_to_string(runtime.join("tmux.calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn s5_b0_failed_buffer_load_cannot_claim_the_requested_payload() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    let (root, attempt, guard) = fixture("tmux", "s5-buffer-failure");
    let _fake = crate::services::tui_prompt_dedupe::binding_context::tests::fake_tmux(root.path());
    std::fs::write(
        root.path().join("tmux"),
        r#"#!/bin/bash
d="$AGENTDESK_ROOT_DIR"
case "$2" in
load-buffer)
  if [ -f "$d/fail-load" ]; then cat >/dev/null; exit 1; fi
  cat > "$d/buffer" ;;
paste-buffer) cat "$d/buffer" >> "$d/wire" ;;
esac
"#,
    )
    .unwrap();
    let runtime = std::path::PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        assert!(
            tmux::load_buffer("s5-buffer", "old")
                .unwrap()
                .status
                .success()
        );
        std::fs::write(runtime.join("fail-load"), "").unwrap();
        assert!(
            !tmux::load_buffer("s5-buffer", "new")
                .unwrap()
                .status
                .success()
        );
        assert!(
            tmux::paste_buffer(&attempt.target.physical, "s5-buffer", false)
                .unwrap()
                .status
                .success()
        );
    }
    assert_eq!(
        std::fs::read_to_string(runtime.join("wire")).unwrap(),
        "old"
    );
    assert!(
        refuses_success(&guard, &attempt, b"new"),
        "failed load must not prove the requested new payload"
    );
    let counts = guard.counts();
    assert_eq!(counts.owner_content.get(&attempt), Some(&1));
    let loads: Vec<_> = counts
        .events
        .iter()
        .filter(|event| event.op == Op::Load)
        .collect();
    assert_eq!(loads.len(), 2);
    assert!(loads[0].written);
    assert!(!loads[1].written);
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        let _effect = producer_effect(
            &attempt.target.physical,
            Op::Paste,
            b"new",
            "APPLIED",
            Some("s5-buffer"),
        );
        assert!(
            SubmissionTmux
                .paste_buffer(&attempt.target.physical, "s5-buffer", false)
                .unwrap()
                .status
                .success()
        );
    }
    assert_eq!(
        std::fs::read_to_string(runtime.join("wire")).unwrap(),
        "oldold"
    );
    let bounded = guard.counts();
    assert_eq!(bounded.owner_content.get(&attempt), Some(&2));
    assert_eq!(
        bounded.events.last().unwrap().bytes,
        0,
        "declared paste also needs a successful buffer load"
    );
    assert_eq!(
        counts
            .events
            .iter()
            .find(|event| event.op == Op::Paste)
            .unwrap()
            .bytes,
        0
    );
}

#[test]
fn s5_b0_bounded_owner_scope_reaches_the_worker_and_observes_paste() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    let (root, attempt, guard) = fixture("tmux", "s5-bounded-owner");
    let _fake = crate::services::tui_prompt_dedupe::binding_context::tests::fake_tmux(root.path());
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        {
            let _effect = producer_effect(
                &attempt.target.physical,
                Op::Paste,
                b"frame",
                "APPLIED",
                None,
            );
            assert!(
                SubmissionTmux
                    .capture(&attempt.target.physical, -10)
                    .is_some()
            );
            assert!(
                guard.counts().owner_content.is_empty(),
                "a control probe cannot inherit producer content"
            );
        }
        assert!(
            SubmissionTmux
                .load_buffer("s5-bounded-buffer", "frame")
                .unwrap()
                .status
                .success()
        );
        assert!(
            SubmissionTmux
                .paste_buffer(&attempt.target.physical, "s5-bounded-buffer", true)
                .unwrap()
                .status
                .success()
        );
    }
    guard.assert_owner_payload(&attempt, b"frame", 1);
    assert!(
        guard
            .counts()
            .events
            .iter()
            .any(|event| event.op == Op::Load && event.written)
    );
}

#[tokio::test]
async fn s5_b0_bounded_pending_drop_keeps_the_physical_writer_until_cleanup() {
    use crate::services::tui_input::bounded_tmux::spawn_bounded;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    let (root, attempt, guard) = fixture("tmux", "s5-bounded-lifetime");
    let script = root.path().join("tmux");
    std::fs::write(&script, "#!/bin/sh\necho reached > \"$S5_RIG/reached\"\nwhile [ ! -f \"$S5_RIG/go\" ]; do sleep .01; done\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = std::process::Command::new(script);
    command.env("S5_RIG", root.path()).args([
        "send-keys",
        "-t",
        &attempt.target.physical,
        "-l",
        "--",
        "frame",
    ]);
    let pending = {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        spawn_bounded(&tokio::runtime::Handle::current(), &mut command).unwrap()
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while !root.path().join("reached").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let mut next = attempt.target.clone();
    next.execution = "execution-B".into();
    assert!(guard.rebind(next.clone()).is_err());
    drop(pending);
    assert!(
        guard.rebind(next.clone()).is_err(),
        "pending drop must not release the live physical writer"
    );
    std::fs::write(root.path().join("go"), "").unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while guard.rebind(next.clone()).is_err() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    guard.assert_owner_payload(&attempt, b"frame", 1);
    assert_eq!(guard.counts().max_writers, 1);
}

#[test]
fn s5_b0_opaque_shell_requires_producer_effect_metadata() {
    use crate::services::tui_input::bounded_tmux::spawn_bounded;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (_root, attempt, guard) = fixture("tmux", "s5-opaque-shell");
    let mut command = std::process::Command::new("/bin/true");
    command.args([
        "if-shell",
        "-t",
        &attempt.target.physical,
        "true",
        "paste-buffer",
    ]);
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        assert!(spawn_bounded(runtime.handle(), &mut command).is_err());
    }
    assert_eq!(guard.counts().attempted_legacy, 1);
    assert!(guard.counts().owner_content.is_empty());
}
