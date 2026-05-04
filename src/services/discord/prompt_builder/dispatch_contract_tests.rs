use super::dispatch_contract::render_dispatch_contract;
use super::manifest::{
    memory_recall_manifest_layer, prompt_manifest_content_sha256, recovery_context_manifest_layer,
};
use super::*;
use crate::db::prompt_manifests::PromptContentVisibility;
use crate::services::discord::settings::MemoryBackendKind;
use crate::services::observability::recovery_audit::{
    RecoveryAuditRecord, recovery_context_sha256,
};

fn build_prompt_with_manifest_for(
    current_task: &CurrentTaskContext<'_>,
    dispatch_type: Option<&str>,
) -> BuiltSystemPrompt {
    build_prompt_with_optional_manifest_for(Some(current_task), dispatch_type)
}

fn build_prompt_with_optional_manifest_for(
    current_task: Option<&CurrentTaskContext<'_>>,
    dispatch_type: Option<&str>,
) -> BuiltSystemPrompt {
    build_system_prompt_with_manifest(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        None,
        false,
        DispatchProfile::Full,
        dispatch_type,
        current_task,
        None,
        None,
        None,
        false,
        None,
        None,
        Some("turn-current-task-test"),
    )
}

#[test]
fn current_task_dispatch_layer_is_recorded_with_redacted_preview_only() {
    let current_task = CurrentTaskContext {
        dispatch_id: Some("dispatch-1534"),
        card_id: Some("card-1534"),
        dispatch_title: Some("Follow up with user@example.com token=super-secret-123"),
        card_title: Some("Instrument current task layer"),
        github_issue_url: Some("https://github.com/itismyfield/AgentDesk/issues/1534"),
        ..CurrentTaskContext::default()
    };

    let built = build_prompt_with_manifest_for(&current_task, Some("implementation"));

    assert!(built.system_prompt.contains("[Current Task]"));
    let manifest = built.manifest.expect("prompt manifest");
    assert_eq!(manifest.layers.len(), 3);
    let layer = manifest
        .layers
        .iter()
        .find(|layer| layer.layer_name == "current_task")
        .expect("current_task layer");
    assert!(layer.enabled);
    assert_eq!(layer.layer_name, "current_task");
    assert_eq!(layer.source.as_deref(), Some("task_dispatches.context"));
    assert_eq!(layer.reason.as_deref(), Some("dispatch_id=dispatch-1534"));
    assert_eq!(
        layer.content_visibility,
        PromptContentVisibility::UserDerived
    );
    assert!(layer.full_content.is_none());

    let preview = layer.redacted_preview.as_deref().unwrap();
    assert!(preview.contains("[redacted-email]"));
    assert!(preview.contains("token=***"));
    assert!(!preview.contains("user@example.com"));
    assert!(!preview.contains("super-secret-123"));

    let serialized = serde_json::to_value(layer).unwrap();
    assert_eq!(serialized["enabled"], true);
    assert_eq!(serialized["full_content"], serde_json::Value::Null);
}

#[test]
fn current_task_freeform_layer_uses_discord_message_source() {
    let current_task = CurrentTaskContext {
        dispatch_title: Some("Manual request from owner@example.com"),
        ..CurrentTaskContext::default()
    };

    let built = build_prompt_with_manifest_for(&current_task, None);

    let manifest = built.manifest.expect("prompt manifest");
    assert_eq!(manifest.layers.len(), 3);
    let layer = manifest
        .layers
        .iter()
        .find(|layer| layer.layer_name == "current_task")
        .expect("current_task layer");
    assert!(layer.enabled);
    assert_eq!(layer.source.as_deref(), Some("discord_message"));
    assert_eq!(layer.reason.as_deref(), Some("freeform"));
    assert!(layer.full_content.is_none());
    assert!(
        layer
            .redacted_preview
            .as_deref()
            .unwrap()
            .contains("[redacted-email]")
    );
}

#[test]
fn dispatch_contract_layer_records_adk_full_content() {
    let current_task = CurrentTaskContext {
        dispatch_id: Some("dispatch-1537"),
        card_title: Some("Instrument dispatch contract layer"),
        ..CurrentTaskContext::default()
    };

    let built = build_prompt_with_manifest_for(&current_task, Some("implementation"));
    let manifest = built.manifest.expect("prompt manifest");
    let layer = manifest
        .layers
        .iter()
        .find(|layer| layer.layer_name == "dispatch_contract")
        .expect("dispatch_contract manifest layer");

    assert!(layer.enabled);
    assert_eq!(
        layer.source.as_deref(),
        Some("prompt_builder.render_dispatch_contract")
    );
    assert_eq!(
        layer.reason.as_deref(),
        Some("dispatch_type=implementation")
    );
    assert!(layer.chars > 0);
    assert!(layer.tokens_est > 0);
    assert_eq!(
        layer.content_visibility,
        PromptContentVisibility::AdkProvided
    );
    assert!(layer.redacted_preview.is_none());
    assert_eq!(
        layer.full_content.as_deref(),
        render_dispatch_contract(Some("implementation"), &current_task).as_deref()
    );
    let full_content = layer.full_content.as_deref().unwrap();
    assert_eq!(
        layer.content_sha256,
        prompt_manifest_content_sha256(full_content)
    );
    assert!(full_content.contains("[Dispatch Contract]"));
    assert!(full_content.contains("`OUTCOME: noop`"));
    assert!(full_content.contains("PATCH /api/dispatches/dispatch-1537"));
}

#[test]
fn dispatch_contract_layer_disabled_for_freeform_without_dispatch() {
    let built = build_prompt_with_optional_manifest_for(None, None);

    assert!(!built.system_prompt.contains("[Dispatch Contract]"));
    let manifest = built.manifest.expect("prompt manifest");
    let layer = manifest
        .layers
        .iter()
        .find(|layer| layer.layer_name == "dispatch_contract")
        .expect("dispatch_contract manifest layer");
    assert!(!layer.enabled);
    assert_eq!(
        layer.source.as_deref(),
        Some("prompt_builder.render_dispatch_contract")
    );
    assert_eq!(layer.reason.as_deref(), Some("dispatch_type=none"));
    assert_eq!(layer.chars, 0);
    assert_eq!(layer.tokens_est, 0);
    assert_eq!(layer.content_sha256, prompt_manifest_content_sha256(""));
    assert_eq!(
        layer.content_visibility,
        PromptContentVisibility::AdkProvided
    );
    assert!(layer.redacted_preview.is_none());
    assert!(layer.full_content.is_none());
}

#[test]
fn recovery_context_layer_records_audit_sha_and_redacted_preview_only() {
    let raw_context = "Alice: email alice@example.com token=secret-value\nAgent: recovered";
    let audit_record = RecoveryAuditRecord {
        id: 7,
        created_at: chrono::Utc::now(),
        channel_id: "42".to_string(),
        session_key: Some("agentdesk-session".to_string()),
        source: "discord_recent".to_string(),
        message_count: 2,
        max_chars_per_message: 300,
        authors: vec!["Alice".to_string(), "Agent".to_string()],
        redacted_preview: "Alice: email ***@*** token=***\nAgent: recovered".to_string(),
        content_sha256: recovery_context_sha256(raw_context),
        consumed_by_turn_id: Some("discord:42:99".to_string()),
    };

    let layer = recovery_context_manifest_layer(Some(&RecoveryContextManifestInput {
        raw_context,
        audit_record: Some(&audit_record),
    }))
    .expect("recovery layer");

    assert_eq!(layer.layer_name, "recovery_context");
    assert!(layer.enabled);
    assert_eq!(layer.source.as_deref(), Some("Discord recent N messages"));
    assert_eq!(
        layer.reason.as_deref(),
        Some("provider-native resume failed")
    );
    assert_eq!(
        layer.content_visibility,
        PromptContentVisibility::UserDerived
    );
    assert_eq!(layer.content_sha256, audit_record.content_sha256);
    assert_eq!(
        layer.redacted_preview.as_deref(),
        Some(audit_record.redacted_preview.as_str())
    );
    assert!(layer.full_content.is_none());
    assert!(
        !layer
            .redacted_preview
            .as_deref()
            .unwrap()
            .contains("secret-value")
    );
    assert!(
        !layer
            .redacted_preview
            .as_deref()
            .unwrap()
            .contains("alice@example.com")
    );
}

#[test]
fn recovery_context_layer_rejects_audit_sha_mismatch() {
    let audit_record = RecoveryAuditRecord {
        id: 7,
        created_at: chrono::Utc::now(),
        channel_id: "42".to_string(),
        session_key: None,
        source: "discord_recent".to_string(),
        message_count: 1,
        max_chars_per_message: 300,
        authors: vec!["Alice".to_string()],
        redacted_preview: "Alice: hello".to_string(),
        content_sha256: "wrong-sha".to_string(),
        consumed_by_turn_id: Some("discord:42:99".to_string()),
    };

    let error = recovery_context_manifest_layer(Some(&RecoveryContextManifestInput {
        raw_context: "Alice: hello",
        audit_record: Some(&audit_record),
    }))
    .expect_err("mismatched audit sha should fail");

    assert!(error.contains("sha256 mismatch"));
}

#[test]
fn build_prompt_manifest_includes_recovery_context_layer() {
    let raw_context = "Alice: token=secret-value\nAgent: recovered";
    let built = build_system_prompt_with_manifest(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        None,
        false,
        DispatchProfile::Full,
        None,
        None,
        None,
        None,
        None,
        false,
        Some(&RecoveryContextManifestInput {
            raw_context,
            audit_record: None,
        }),
        None,
        Some("turn-recovery-context-test"),
    );

    let manifest = built.manifest.expect("prompt manifest");
    let layer = manifest
        .layers
        .iter()
        .find(|layer| layer.layer_name == "recovery_context")
        .expect("recovery_context layer");
    assert!(layer.enabled);
    let expected_sha = recovery_context_sha256(raw_context);
    assert_eq!(layer.content_sha256, expected_sha);
    assert!(layer.full_content.is_none());
    assert!(
        layer
            .redacted_preview
            .as_deref()
            .unwrap()
            .contains("token=***")
    );
    assert!(
        !layer
            .redacted_preview
            .as_deref()
            .unwrap()
            .contains("secret-value")
    );

    assert_eq!(
        layer.content_visibility,
        PromptContentVisibility::UserDerived
    );
}

#[test]
fn recovery_context_layer_disabled_when_absent() {
    let layer = recovery_context_manifest_layer(None).expect("disabled recovery layer");

    assert_eq!(layer.layer_name, "recovery_context");
    assert!(!layer.enabled);
    assert_eq!(layer.source.as_deref(), Some("Discord recent N messages"));
    assert_eq!(
        layer.reason.as_deref(),
        Some("provider-native resume failed")
    );
    assert_eq!(layer.chars, 0);
    assert_eq!(layer.tokens_est, 0);
    assert_eq!(layer.content_sha256, prompt_manifest_content_sha256(""));
    assert!(layer.full_content.is_none());
    assert!(layer.redacted_preview.is_none());
}

#[test]
fn memory_recall_layer_records_memento_preview_only() {
    let settings = ResolvedMemorySettings {
        backend: MemoryBackendKind::Memento,
        ..ResolvedMemorySettings::default()
    };
    let recall = MemoryRecallManifestInput {
        should_recall: true,
        gate_reason: "previous_context_signal",
        external_recall: Some(
            "[External Recall]\nUser email owner@example.com token=private-token-123",
        ),
    };

    let layer = memory_recall_manifest_layer(Some(&settings), true, Some(&recall))
        .expect("memory recall layer");

    assert_eq!(layer.layer_name, "memory_recall");
    assert!(layer.enabled);
    assert_eq!(layer.source.as_deref(), Some("memento"));
    assert_eq!(
        layer.reason.as_deref(),
        Some("memory_backend=memento;recall=previous_context_signal")
    );
    assert_eq!(
        layer.content_visibility,
        PromptContentVisibility::UserDerived
    );
    assert!(layer.full_content.is_none());
    assert!(layer.chars > 0);
    assert!(layer.tokens_est > 0);
    assert_eq!(
        layer.content_sha256,
        prompt_manifest_content_sha256(
            "[External Recall]\nUser email owner@example.com token=private-token-123"
        )
    );

    let preview = layer.redacted_preview.as_deref().expect("preview");
    assert!(preview.contains("[redacted-email]"));
    assert!(preview.contains("token=***"));
    assert!(!preview.contains("owner@example.com"));
    assert!(!preview.contains("private-token-123"));
}

#[test]
fn memory_recall_layer_disabled_when_recall_skipped() {
    let settings = ResolvedMemorySettings {
        backend: MemoryBackendKind::Memento,
        ..ResolvedMemorySettings::default()
    };
    let recall = MemoryRecallManifestInput {
        should_recall: false,
        gate_reason: "no_turn_signal",
        external_recall: Some("raw memory that must not be stored"),
    };

    let layer = memory_recall_manifest_layer(Some(&settings), true, Some(&recall))
        .expect("memory recall layer");

    assert_eq!(layer.layer_name, "memory_recall");
    assert!(!layer.enabled);
    assert_eq!(layer.source.as_deref(), Some("memento"));
    assert_eq!(
        layer.reason.as_deref(),
        Some("memory_backend=memento;recall_skipped=no_turn_signal")
    );
    assert_eq!(layer.chars, 0);
    assert_eq!(layer.tokens_est, 0);
    assert_eq!(layer.content_sha256, prompt_manifest_content_sha256(""));
    assert!(layer.full_content.is_none());
    assert!(layer.redacted_preview.is_none());
}

#[test]
fn memory_recall_layer_disabled_when_memento_backend_disabled() {
    let settings = ResolvedMemorySettings {
        backend: MemoryBackendKind::File,
        ..ResolvedMemorySettings::default()
    };
    let recall = MemoryRecallManifestInput {
        should_recall: true,
        gate_reason: "non_memento_backend",
        external_recall: Some("raw memory that must not be stored"),
    };

    let layer = memory_recall_manifest_layer(Some(&settings), true, Some(&recall))
        .expect("memory recall layer");

    assert_eq!(layer.layer_name, "memory_recall");
    assert!(!layer.enabled);
    assert_eq!(layer.source.as_deref(), Some("memento"));
    assert_eq!(layer.reason.as_deref(), Some("memory_backend=file"));
    assert_eq!(layer.chars, 0);
    assert_eq!(layer.tokens_est, 0);
    assert!(layer.full_content.is_none());
    assert!(layer.redacted_preview.is_none());
}

#[test]
fn phase_gate_contract_requires_verdict_and_checks() {
    let dispatch_context = serde_json::json!({
        "phase_gate": {
            "pass_verdict": "phase_gate_passed",
            "checks": ["merge_verified", "issue_closed", "build_passed"]
        }
    });
    let dispatch_context_raw = dispatch_context.to_string();
    let current_task = CurrentTaskContext {
        dispatch_id: Some("dispatch-phase-gate-1"),
        dispatch_context: Some(&dispatch_context_raw),
        ..CurrentTaskContext::default()
    };
    let contract = render_dispatch_contract(Some("phase-gate"), &current_task)
        .expect("phase-gate dispatch contract");

    assert!(contract.contains("PATCH /api/dispatches/dispatch-phase-gate-1"));
    assert!(contract.contains("result.verdict는 반드시 `phase_gate_passed`"));
    assert!(contract.contains("\"verdict\":\"phase_gate_passed\""));
    assert!(contract.contains("\"merge_verified\":{\"status\":\"pass\"}"));
    assert!(contract.contains("review verdict API는 사용하지 않는다"));
}

#[test]
fn review_lite_prompt_keeps_review_contract_while_trimming_full_sections() {
    use super::super::settings::RoleBinding;

    let binding = RoleBinding {
        role_id: "project-agentdesk".to_string(),
        prompt_file: "/nonexistent".to_string(),
        provider: None,
        model: None,
        reasoning_effort: None,
        peer_agents_enabled: true,
        quality_feedback_injection_enabled: true,
        memory: Default::default(),
    };
    let dispatch_context = serde_json::json!({
        "repo": "itismyfield/AgentDesk",
        "issue_number": 1473,
        "review_quality_scope_reminder": "Review only the requested change and directly related regressions.",
        "review_verdict_guidance": "Use improve when actionable regressions are found.",
        "verdict_endpoint": "POST /api/reviews/verdict"
    });
    let dispatch_context_raw = dispatch_context.to_string();
    let current_task = CurrentTaskContext {
        dispatch_id: Some("dispatch-review-1473"),
        card_id: Some("card-1473"),
        dispatch_title: Some("[Review] #1473"),
        dispatch_context: Some(&dispatch_context_raw),
        card_title: Some("trim review MCP catalog"),
        github_issue_url: Some("https://github.com/itismyfield/AgentDesk/issues/1473"),
    };
    let shared_knowledge = Some("[Shared Agent Knowledge]\n".repeat(80));
    let longterm_catalog = Some("- memory.md: detailed operational memory\n".repeat(80));

    let full_prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        Some(&binding),
        false,
        DispatchProfile::Full,
        Some("implementation"),
        Some(&current_task),
        shared_knowledge.as_deref(),
        longterm_catalog.as_deref(),
        Some(&ResolvedMemorySettings {
            backend: MemoryBackendKind::File,
            ..ResolvedMemorySettings::default()
        }),
        false,
    );
    let review_prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        Some(&binding),
        false,
        DispatchProfile::ReviewLite,
        Some("review"),
        Some(&current_task),
        shared_knowledge.as_deref(),
        longterm_catalog.as_deref(),
        Some(&ResolvedMemorySettings {
            backend: MemoryBackendKind::File,
            ..ResolvedMemorySettings::default()
        }),
        false,
    );

    assert!(review_prompt.contains("[Review Rules]"));
    assert!(review_prompt.contains("Review Scope Reminder"));
    assert!(review_prompt.contains("Review Verdict Guidance"));
    assert!(review_prompt.contains("Verdict Endpoint: POST /api/reviews/verdict"));
    assert!(!review_prompt.contains("[Long-term Memory]"));
    assert!(!review_prompt.contains("[Proactive Memory Guidance]"));

    let review_words = review_prompt.split_whitespace().count();
    let full_words = full_prompt.split_whitespace().count();
    if std::env::var_os("AGENTDESK_PRINT_REVIEW_LITE_BASELINE").is_some() {
        eprintln!(
            "review_lite_prompt_baseline review_chars={} review_words={} full_chars={} full_words={}",
            review_prompt.len(),
            review_words,
            full_prompt.len(),
            full_words
        );
    }
    assert!(review_words * 2 < full_words);
}
