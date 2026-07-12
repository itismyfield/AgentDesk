use super::dispatch_contract::render_dispatch_contract;
use super::manifest::{
    memory_recall_manifest_layer, prompt_manifest_content_sha256, recovery_context_manifest_layer,
    role_prompt_manifest_layer, truncate_prompt_manifest_preview,
};
use super::*;
use crate::db::prompt_manifests::PromptContentVisibility;
use crate::services::discord::settings::{MemoryBackendKind, RoleBinding};
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

fn test_role_binding(role_id: &str) -> RoleBinding {
    RoleBinding {
        role_id: role_id.to_string(),
        prompt_file: "/nonexistent".to_string(),
        provider: None,
        model: None,
        reasoning_effort: None,
        peer_agents_enabled: true,
        quality_feedback_injection_enabled: true,
        memory: Default::default(),
    }
}

#[test]
fn prompt_manifest_log_records_hash_metadata_without_full_content() {
    // The `info!("recorded prompt manifest", ...)` call logs the manifest's
    // `turn_id`/`channel_id`/`layer_count` plus the `?layer_hashes` field whose
    // value is the Debug render of `prompt_manifest_layer_hashes(...)`. Asserting
    // on that pure value (instead of capturing subscriber output) is
    // deterministic — a global tracing callsite-interest race in the 3k-test
    // binary otherwise leaves a captured buffer empty under parallel execution.
    let current_task = CurrentTaskContext {
        dispatch_id: Some("dispatch-log-secret"),
        card_title: Some("Instrument prompt manifest logging"),
        ..CurrentTaskContext::default()
    };
    let built = build_prompt_with_manifest_for(&current_task, Some("implementation"));

    let manifest = built.manifest.expect("prompt manifest");
    let dispatch_layer = manifest
        .layers
        .iter()
        .find(|layer| layer.layer_name == "dispatch_contract")
        .expect("dispatch_contract manifest layer");
    let full_content = dispatch_layer.full_content.as_deref().unwrap();
    let sensitive_contract_fragment = "PATCH /api/dispatches/dispatch-log-secret";
    // Full content stays in the manifest for storage/audit even though it is
    // never emitted to the log surface below.
    assert!(full_content.contains(sensitive_contract_fragment));

    // Exactly the metadata logged at info level — no full_content field exists.
    let layer_hashes = format!("{:?}", prompt_manifest_layer_hashes(&manifest));
    assert_eq!(manifest.layers.len(), 7);
    assert_eq!(manifest.layer_count, 6);
    assert_eq!(
        manifest.total_input_bytes,
        i64::try_from(built.system_prompt.len()).unwrap()
    );
    for required in [
        "base_discord",
        "tool_output_efficiency",
        "context_compression",
        "api_friction_guidance",
        "current_task",
        "dispatch_contract",
    ] {
        assert!(
            manifest
                .layers
                .iter()
                .any(|layer| layer.enabled && layer.layer_name == required),
            "missing enabled layer {required}"
        );
    }
    assert_eq!(manifest.turn_id, "turn-current-task-test");
    assert!(layer_hashes.contains("dispatch_contract"));
    assert!(layer_hashes.contains(&dispatch_layer.content_sha256));
    for layer in &manifest.layers {
        assert!(layer_hashes.contains(layer.layer_name.as_str()));
        assert!(layer_hashes.contains(layer.content_sha256.as_str()));
    }
    // The logged hash list must never carry the body or any sensitive fragment.
    assert!(!layer_hashes.contains("full_content"));
    assert!(!layer_hashes.contains(sensitive_contract_fragment));
    assert!(!layer_hashes.contains("PATCH /api/dispatches"));
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
    assert_eq!(manifest.layers.len(), 7);
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
    assert!(!preview.contains("[Dispatch Contract]"));
    assert!(!preview.contains("user@example.com"));
    assert!(!preview.contains("super-secret-123"));

    let serialized = serde_json::to_value(layer).unwrap();
    assert_eq!(serialized["enabled"], true);
    assert_eq!(serialized["full_content"], serde_json::Value::Null);
}

#[test]
fn full_prompt_manifest_records_shared_knowledge_and_longterm_catalog() {
    let binding = test_role_binding("manifest-inventory-agent");
    let built = build_system_prompt_with_manifest(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        Some(&binding),
        false,
        DispatchProfile::Full,
        None,
        None,
        Some("[Shared Agent Knowledge]\nimportant invariant"),
        Some("- memory.md: durable fact"),
        None,
        false,
        None,
        None,
        Some("turn-layer-inventory"),
    );

    let manifest = built.manifest.expect("prompt manifest");
    for (name, expected_fragment) in [
        (
            "base_discord",
            "You are chatting with a user through Discord.",
        ),
        ("shared_knowledge", "important invariant"),
        ("longterm_catalog", "durable fact"),
    ] {
        let layer = manifest
            .layers
            .iter()
            .find(|layer| layer.layer_name == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        assert!(layer.enabled, "{name} should describe injected content");
        let recorded = layer
            .full_content
            .as_deref()
            .or(layer.redacted_preview.as_deref())
            .expect("recorded content");
        assert!(recorded.contains(expected_fragment));
    }
    assert_eq!(
        manifest.layer_count as usize,
        manifest.layers.iter().filter(|layer| layer.enabled).count()
    );
}

#[test]
fn current_task_freeform_layer_uses_discord_message_source() {
    let current_task = CurrentTaskContext {
        dispatch_title: Some("Manual request from owner@example.com"),
        ..CurrentTaskContext::default()
    };

    let built = build_prompt_with_manifest_for(&current_task, None);

    let manifest = built.manifest.expect("prompt manifest");
    assert_eq!(manifest.layers.len(), 7);
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
    assert_eq!(
        layer.full_content.as_deref(),
        render_dispatch_contract(Some("implementation"), &current_task).as_deref()
    );
    let full_content = layer.full_content.as_deref().unwrap();
    assert_eq!(
        layer.redacted_preview.as_deref(),
        Some(truncate_prompt_manifest_preview(full_content, 200).as_str())
    );
    assert_eq!(
        layer.content_sha256,
        prompt_manifest_content_sha256(full_content)
    );
    assert!(full_content.contains("[Dispatch Contract]"));
    assert!(full_content.contains("`OUTCOME: noop`"));
    assert!(full_content.contains("PATCH /api/dispatches/dispatch-1537"));
}

#[test]
fn role_prompt_layer_records_adk_preview_while_preserving_full_content() {
    let binding = test_role_binding("project-agentdesk");
    let full_content = format!(
        "{}TAIL SECTION MUST REMAIN ONLY IN full_content",
        "Role prompt authoritative source. ".repeat(12)
    );
    let expected_preview = truncate_prompt_manifest_preview(&full_content, 200);

    let layer = role_prompt_manifest_layer(&binding, true, Some(full_content.clone()));

    assert_eq!(layer.layer_name, "role_prompt");
    assert!(layer.enabled);
    assert_eq!(
        layer.content_visibility,
        PromptContentVisibility::AdkProvided
    );
    assert_eq!(layer.full_content.as_deref(), Some(full_content.as_str()));
    assert_eq!(
        layer.redacted_preview.as_deref(),
        Some(expected_preview.as_str())
    );
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
    assert_eq!(
        manifest.total_input_bytes,
        i64::try_from(built.system_prompt.len() + raw_context.len()).unwrap()
    );
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
fn scope_assessment_contract_requires_scope_depth_and_patch_completion() {
    // #3605 (T2): the scope-assessment contract must instruct the agent to
    // evaluate scale only, emit one of full|plan_only|direct, and complete via
    // the standard PATCH path.
    let current_task = CurrentTaskContext {
        dispatch_id: Some("dispatch-scope-1"),
        ..CurrentTaskContext::default()
    };
    let contract = render_dispatch_contract(Some("scope-assessment"), &current_task)
        .expect("scope-assessment dispatch contract");

    assert!(contract.contains("[Dispatch Contract]"));
    assert!(contract.contains("PATCH /api/dispatches/dispatch-scope-1"));
    // #3605 (T2): the contract must affirmatively forbid implementation, not
    // merely mention the word "구현". A bare `contains("구현")` would also pass a
    // contract that *instructs* the agent to implement (e.g. "구현하라"), so
    // assert the explicit "evaluate only / do not implement" directive verbatim.
    assert!(
        contract.contains("구현/수정/커밋은 하지 않는다"),
        "scope-assessment contract must explicitly forbid implementation, got: {contract}"
    );
    assert!(
        contract.contains("\"범위 평가\" 전용"),
        "scope-assessment contract must declare it is evaluation-only, got: {contract}"
    );
    // Negative guard: it must NOT carry an implementation directive.
    assert!(
        !contract.contains("구현하라") && !contract.contains("구현한다"),
        "scope-assessment contract must not instruct the agent to implement, got: {contract}"
    );
    // All three depth labels are documented.
    assert!(contract.contains("full"));
    assert!(contract.contains("plan_only"));
    assert!(contract.contains("direct"));
    // The structured result keys are required.
    assert!(contract.contains("scope_depth"));
    assert!(contract.contains("scope_reason"));
    assert!(contract.contains("scope_risk"));
    assert!(contract.contains("review verdict API는 사용하지 않는다"));
}

#[test]
fn parent_plan_context_is_rendered_into_the_current_task_section() {
    // #3594 (T3, codex Finding 3): a plan-review (or full→impl) dispatch carries
    // the parent plan body in its context under `parent_plan`. It must surface in
    // the [Current Task] / [Dispatch Context] block so the agent actually sees the
    // plan it must review / implement.
    use super::dispatch_contract::render_current_task_section;

    let dispatch_context = serde_json::json!({
        "auto_queue": true,
        "scope_depth": "full",
        "parent_plan": "Design: split module X.\nSteps: 1) extract Y 2) wire Z"
    });
    let dispatch_context_raw = dispatch_context.to_string();
    let current_task = CurrentTaskContext {
        dispatch_id: Some("dispatch-plan-review-1"),
        dispatch_context: Some(&dispatch_context_raw),
        ..CurrentTaskContext::default()
    };

    let section = render_current_task_section(&current_task, Some("plan-review"))
        .expect("current task section");
    assert!(
        section.contains("Parent Plan (from the plan dispatch):"),
        "the parent plan must be labelled in the prompt, got: {section}"
    );
    assert!(
        section.contains("Design: split module X.")
            && section.contains("Steps: 1) extract Y 2) wire Z"),
        "the full plan body must be rendered verbatim, got: {section}"
    );
}

#[test]
fn parent_plan_is_truncated_when_oversized() {
    // Defensive: an oversized plan must be truncated so it cannot blow up the
    // system prompt, with an explicit truncation marker.
    use super::dispatch_contract::render_current_task_section;

    let huge_plan = "x".repeat(20_000);
    let dispatch_context = serde_json::json!({ "parent_plan": huge_plan });
    let dispatch_context_raw = dispatch_context.to_string();
    let current_task = CurrentTaskContext {
        dispatch_id: Some("dispatch-plan-review-2"),
        dispatch_context: Some(&dispatch_context_raw),
        ..CurrentTaskContext::default()
    };

    let section = render_current_task_section(&current_task, Some("plan-review"))
        .expect("current task section");
    assert!(
        section.contains("… (plan truncated)"),
        "oversized plan must be truncated with a marker"
    );
    // The rendered plan portion must be far smaller than the original 20k chars.
    assert!(
        section.chars().count() < 12_000,
        "truncated section must be bounded, got {} chars",
        section.chars().count()
    );
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

#[test]
fn full_memento_prompt_carries_tool_feedback_contract() {
    // #4306: the Proactive Memory Guidance memento branch must carry the
    // always-on `tool_feedback` contract that was dropped during the da7ccb39
    // provider-prompt slim-down. It is gated to the Full profile with the
    // memento backend and the MCP available.
    let settings = ResolvedMemorySettings {
        backend: MemoryBackendKind::Memento,
        ..ResolvedMemorySettings::default()
    };
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp/agentdesk",
        ChannelId::new(1),
        "tok",
        None,
        false,
        DispatchProfile::Full,
        None,
        None,
        None,
        None,
        Some(&settings),
        true,
    );

    assert!(
        prompt.contains("[Proactive Memory Guidance]"),
        "Full+memento prompt must include the proactive memory guidance block, got: {prompt}"
    );
    assert!(
        prompt.contains(
            "feedback contract: in the same turn you use `recall`/`context` results, \
             call `mcp__memento__tool_feedback` once"
        ),
        "Full+memento prompt must carry the tool_feedback contract, got: {prompt}"
    );
    // Required params surfaced verbatim per the current memento schema:
    // required = tool_name/relevant/sufficient; search_event_id is optional
    // (recommended when the response carries _meta.searchEventId).
    assert!(prompt.contains("required: `tool_name`, `relevant`, `sufficient`"));
    assert!(prompt.contains(
        "when the response carries `_meta.searchEventId`, \
         also pass it as `search_event_id` — recommended"
    ));
    // Deferred-tool loading instruction.
    assert!(prompt.contains("ToolSearch `select:mcp__memento__tool_feedback`"));
}

#[test]
fn review_lite_and_lite_prompts_omit_tool_feedback_contract() {
    // #4306: the tool_feedback contract lives inside the Full-only Proactive
    // Memory Guidance block. ReviewLite/Lite must show zero output change — the
    // whole block (and thus the contract) stays absent even with the memento
    // backend selected and the MCP available.
    let settings = ResolvedMemorySettings {
        backend: MemoryBackendKind::Memento,
        ..ResolvedMemorySettings::default()
    };

    for profile in [DispatchProfile::ReviewLite, DispatchProfile::Lite] {
        let dispatch_type = match profile {
            DispatchProfile::ReviewLite => Some("review"),
            _ => None,
        };
        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/tmp/agentdesk",
            ChannelId::new(1),
            "tok",
            None,
            false,
            profile,
            dispatch_type,
            None,
            None,
            None,
            Some(&settings),
            true,
        );
        assert!(
            !prompt.contains("[Proactive Memory Guidance]"),
            "{profile:?} prompt must not include the proactive memory guidance block, got: {prompt}"
        );
        assert!(
            !prompt.contains("mcp__memento__tool_feedback"),
            "{profile:?} prompt must not carry the tool_feedback contract, got: {prompt}"
        );
    }
}

#[test]
fn foreign_workspace_full_prompt_omits_repo_relative_doc_paths() {
    // #4314 (end-to-end anchor): a Full-profile agent whose cwd is NOT an
    // AgentDesk checkout (no docs/source-of-truth.md / docs/memory-scope.md
    // under it) must never get the repo-relative doc references injected into
    // its assembled system prompt — neither via the Shared Agent Rules Index
    // nor via the Proactive Memory Guidance memento branch. The generic
    // shared-rules block and the absolute `_shared.prompt.md` source line stay.
    let binding = test_role_binding("project-cookingheart");
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/nonexistent-foreign-workspace-4314", // no docs/*.md rooted here
        ChannelId::new(1),
        "tok",
        Some(&binding),
        false,
        DispatchProfile::Full,
        Some("implementation"),
        None,
        None,
        None,
        Some(&ResolvedMemorySettings {
            backend: MemoryBackendKind::Memento,
            ..ResolvedMemorySettings::default()
        }),
        true,
    );

    assert!(
        prompt.contains("[Shared Agent Rules Index]"),
        "generic shared-rules block must stay, got: {prompt}"
    );
    assert!(
        prompt.contains("_shared.prompt.md"),
        "absolute shared-prompt source line must stay, got: {prompt}"
    );
    assert!(
        !prompt.contains("docs/source-of-truth.md"),
        "foreign workspace must not reference docs/source-of-truth.md, got: {prompt}"
    );
    assert!(
        !prompt.contains("docs/memory-scope.md"),
        "foreign workspace must not reference docs/memory-scope.md, got: {prompt}"
    );
}
