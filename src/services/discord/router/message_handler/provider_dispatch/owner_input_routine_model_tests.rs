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
