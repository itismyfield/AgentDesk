use super::*;
use crate::services::tui_input::routine_model::{Requirement, prepared, scope};

#[test]
fn a_persistent_model_guard_reaches_the_owner_idle_effect_before_keys() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("rm-owner", 8_219_301, "normal", view("", false), IDLE);
    let applied = Requirement {
        provider: "claude".into(),
        tmux: rig.name.clone(),
        model: "sonnet".into(),
        resume: None,
    };
    scope(applied.clone(), || {
        prepared(
            "claude",
            &rig.name,
            "native",
            false,
            &rig.nonce,
            &["--model".into(), "sonnet".into()],
        )
        .unwrap();
    });
    let requested = Requirement {
        model: "opus".into(),
        ..applied
    };
    let result = scope(requested, || {
        rig.dispatch(ProviderKind::Claude, target::Phase::Idle, &rig.transcript())
    });
    assert!(
        matches!(&result, Err(reason) if reason.contains("NotSent")),
        "a different applied model cannot accept this prompt: {result:?}"
    );
    assert!(
        rig.events().is_empty(),
        "no key or content may reach the old model"
    );
    let snapshot = rig.snapshot();
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(
        snapshot["items"].as_array().map(Vec::len),
        Some(1),
        "{snapshot}"
    );
    assert_eq!(snapshot["items"][0]["message_id"], rig.original.message_id);
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
}

#[test]
fn routine_fix2_claude_fallback_is_not_sent_by_the_owner() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dialog = "You've hit your usage limit for the current model.\n\
        ❯ 1. Continue with the fallback model\n\
        2. Wait until reset\nEnter to confirm · Esc to cancel\n";
    for strict in [false, true] {
        let id = 8_219_320 + u64::from(strict);
        let rig = rig(
            &format!("rm-fallback-{id}"),
            id,
            "startup",
            dialog.into(),
            IDLE,
        );
        let _driver = rig.trace_driver();
        startup_repaint_fixture(&rig, "Continue with the fallback model");
        let requirement = Requirement {
            provider: "claude".into(),
            tmux: rig.name.clone(),
            model: "opus".into(),
            resume: Some("native".into()),
        };
        scope(requirement.clone(), || {
            prepared(
                "claude",
                &rig.name,
                "native",
                true,
                &rig.nonce,
                &[
                    "--resume".into(),
                    "native".into(),
                    "--model".into(),
                    "opus".into(),
                ],
            )
            .unwrap();
        });
        let applied = || {
            crate::services::tui_input::routine_model::matches(
                "claude", &rig.name, &rig.nonce, "native", "opus",
            )
        };
        assert!(applied());
        let result = if strict {
            scope(requirement, || created_first_prompt(&rig))
        } else {
            created_first_prompt(&rig)
        };
        let trace = fs::read_to_string(rig.f.root.path().join(format!("{}-owner/trace", rig.name)))
            .unwrap();
        if strict {
            assert!(
                rig.events().is_empty(),
                "strict fallback sent keys: {:?}; {result:?}",
                rig.events()
            );
            assert!(!trace.contains("load-buffer"), "{trace}");
            assert!(
                matches!(&result, Err(reason) if reason.contains("NotSent") && reason.contains("routine_model_fallback")),
                "{result:?}"
            );
            let state = rig.snapshot();
            assert!(state["head_injection"].is_null(), "{state}");
            assert_eq!(state["items"].as_array().unwrap().len(), 1, "{state}");
            assert_eq!(state["items"][0]["message_id"], rig.original.message_id);
        } else {
            rig.assert_hooks_completed();
            assert!(
                !applied(),
                "ordinary fallback must revoke the previous model proof"
            );
            assert_eq!(
                rig.events(),
                ["Enter"],
                "ordinary fallback still accepts Enter"
            );
            assert!(
                trace.contains("load-buffer"),
                "ordinary fallback advances to the prompt: {result:?}"
            );
        }
    }
}
