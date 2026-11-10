use super::*;
use crate::services::discord::commands::model_ui::build_model_picker_option_specs;

const SESSION: &str = "80360000-0000-4000-8000-000000000001";
const MODEL: &str = "claude-fable-5-2";

fn options() -> Vec<crate::services::discord::commands::model_ui::ModelPickerOptionSpec> {
    build_model_picker_option_specs(
        &ProviderKind::Claude,
        None,
        None,
        "default",
        "default",
        None,
    )
}

async fn observe(root: &Path, selector: &str, model: &str, error: bool, session: &str) {
    let records = root.join("records");
    std::fs::create_dir_all(&records).unwrap();
    let record =
        crate::services::claude_tui::hook_bundle::statusline::record_path(&records, SESSION);
    std::fs::write(
        record,
        serde_json::json!({"session_id": SESSION, "written_at": 2.0}).to_string(),
    )
    .unwrap();
    let old = format!(
        "{}\n",
        serde_json::json!({"type":"assistant", "sessionId":SESSION,
        "message":{"model":"claude-old"}})
    );
    let foreign = serde_json::json!({"type":"assistant", "sessionId":"other-session",
        "message":{"model":"claude-foreign"}});
    let answer = serde_json::json!({"type":"assistant", "sessionId":session,
        "isApiErrorMessage":error, "message":{"model":model}});
    let transcript = root.join("transcript.jsonl");
    std::fs::write(&transcript, format!("{old}{foreign}\n{answer}\n")).unwrap();
    let confirm = Confirm {
        session: SESSION.to_string(),
        requested_fast: None,
        at: 1.0,
        model: Some(ModelCheck {
            requested: Some(selector.to_string()),
            effort: None,
            transcript: Some(transcript),
            offset: old.len() as u64,
        }),
    };
    let waits = Waits {
        record: Duration::ZERO,
        settle: Duration::ZERO,
        poll: Duration::ZERO,
        answer: Duration::ZERO,
    };
    confirm_notice(
        &confirm,
        &records,
        || async { Some(SESSION.to_string()) },
        &waits,
    )
    .await;
}

#[tokio::test]
async fn model_catalog_observation_reaches_actual_model_picker() {
    let root = crate::config::TestRuntimeRootGuard::new();
    observe(root.path(), "fable", MODEL, false, SESSION).await;
    let entries = options();
    let alias = entries.iter().find(|entry| entry.value == "fable").unwrap();
    assert_eq!(alias.label, format!("Fable · {MODEL}"));
    assert!(entries.iter().any(|entry| entry.value == MODEL));
    assert!(
        !entries
            .iter()
            .any(|entry| entry.value == "claude-old" || entry.value == "claude-foreign")
    );
    assert!(entries.len() <= 25);
    assert!(
        entries
            .iter()
            .all(|entry| entry.label.chars().count() <= 100
                && crate::services::discord::model_catalog::is_valid_discord_select_option_value(
                    &entry.value
                ))
    );
    observe(root.path(), "fable", "claude-fable-next", false, SESSION).await;
    assert_eq!(
        options()
            .iter()
            .find(|entry| entry.value == "fable")
            .unwrap()
            .label,
        "Fable · claude-fable-next"
    );
}

#[test]
fn model_catalog_observation_missing_and_corrupt_files_keep_unversioned_aliases() {
    let root = crate::config::TestRuntimeRootGuard::new();
    let path = root.path().join("runtime/claude_model_observations.json");
    for raw in [None, Some("{"), Some("[]")] {
        if let Some(raw) = raw {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, raw).unwrap();
        }
        let entries = options();
        for (selector, label) in [
            ("fable", "Fable"),
            ("opus", "Opus"),
            ("sonnet", "Sonnet"),
            ("haiku", "Haiku"),
        ] {
            assert_eq!(
                entries
                    .iter()
                    .find(|entry| entry.value == selector)
                    .unwrap()
                    .label,
                label
            );
        }
    }
}

#[tokio::test]
async fn model_catalog_observation_excludes_synthetic_errors_nonaliases_and_foreign_sessions() {
    let root = crate::config::TestRuntimeRootGuard::new();
    let path = root.path().join("runtime/claude_model_observations.json");
    for (selector, model, error, session) in [
        ("fable", "<synthetic>", false, SESSION),
        ("fable", MODEL, true, SESSION),
        (MODEL, MODEL, false, SESSION),
        ("opusplan", MODEL, false, SESSION),
        ("fable", MODEL, false, "other-session"),
    ] {
        observe(root.path(), selector, model, error, session).await;
        assert!(
            !path.exists(),
            "must exclude {selector} / {model} / error={error} / {session}"
        );
    }
}
