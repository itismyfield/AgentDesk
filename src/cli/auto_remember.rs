use serde_json::json;

use crate::services::memory::{
    AutoRememberAuditDetail, AutoRememberAuditFilter, AutoRememberMemoryStatus, AutoRememberStage,
    AutoRememberStore, reject_auto_remember_candidate, requeue_auto_remember_candidate,
    resubmit_auto_remember_candidate, verify_auto_remember_candidate,
};

pub(crate) fn cmd_auto_remember_audit(
    workspace: Option<&str>,
    status: Option<AutoRememberMemoryStatus>,
    stage: Option<AutoRememberStage>,
    signal_kind: Option<&str>,
    candidate_hash: Option<&str>,
    resubmittable_only: bool,
    limit: usize,
    json_output: bool,
) -> Result<(), String> {
    let store = AutoRememberStore::open_existing()?
        .ok_or_else(|| "auto-remember sidecar does not exist for this runtime root".to_string())?;
    let records = store.list_audit_filtered(&AutoRememberAuditFilter {
        workspace,
        status,
        stage,
        signal_kind,
        candidate_hash,
        resubmittable_only,
        limit,
    })?;

    if json_output {
        let payload = records.iter().map(audit_detail_json).collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!(payload))
                .map_err(|err| format!("failed to encode auto-remember audit json: {err}"))?
        );
        return Ok(());
    }

    if records.is_empty() {
        println!("No auto-remember audit rows found.");
        return Ok(());
    }

    for record in &records {
        println!(
            "{}  {}  {}  {}  {}",
            record.created_at,
            record.workspace,
            record.signal_kind,
            record.status.as_str(),
            truncate_hash(&record.candidate_hash),
        );
        println!(
            "  stage={} retry_count={} turn_id={}",
            record.stage.as_str(),
            record.retry_count,
            record.turn_id,
        );
        if let Some(raw_content) = record.raw_content.as_deref() {
            println!("  raw={raw_content}");
        }
        if let Some(entity_key) = record.entity_key.as_deref() {
            println!("  entity_key={entity_key}");
        }
        if let Some(error) = record.error.as_deref() {
            println!("  note={error}");
        }
    }

    Ok(())
}

pub(crate) fn cmd_auto_remember_summary(
    workspace: Option<&str>,
    json_output: bool,
) -> Result<(), String> {
    let store = AutoRememberStore::open_existing()?
        .ok_or_else(|| "auto-remember sidecar does not exist for this runtime root".to_string())?;
    let status_counts = store.count_by_status(workspace)?;
    let skip_reason_counts = store.count_validation_skip_reasons(workspace)?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "workspace": workspace,
                "status_counts": status_counts
                    .iter()
                    .map(|(status, count)| json!({"status": status, "count": count}))
                    .collect::<Vec<_>>(),
                "validation_skip_reasons": skip_reason_counts
                    .iter()
                    .map(|(reason, count)| json!({"reason": reason, "count": count}))
                    .collect::<Vec<_>>(),
            }))
            .map_err(|err| format!("failed to encode auto-remember summary json: {err}"))?
        );
        return Ok(());
    }

    println!("Auto-remember status counts:");
    if status_counts.is_empty() {
        println!("  none");
    } else {
        for (status, count) in &status_counts {
            println!("  {status}: {count}");
        }
    }

    println!("Validation skip reasons:");
    if skip_reason_counts.is_empty() {
        println!("  none");
    } else {
        for (reason, count) in &skip_reason_counts {
            println!("  {reason}: {count}");
        }
    }

    Ok(())
}

pub(crate) async fn cmd_auto_remember_resubmit(
    workspace: &str,
    candidate_hash: &str,
) -> Result<(), String> {
    let result = resubmit_auto_remember_candidate(workspace, candidate_hash).await?;
    println!(
        "Auto-remember resubmit finished: remembered={} duplicates={} input_tokens={} output_tokens={}",
        result.remembered_count,
        result.duplicate_count,
        result.token_usage.input_tokens,
        result.token_usage.output_tokens,
    );
    for warning in &result.warnings {
        println!("warning: {warning}");
    }

    if result.remembered_count == 0 && !result.warnings.is_empty() {
        return Err(result.warnings.join(" | "));
    }
    Ok(())
}

pub(crate) fn cmd_auto_remember_verify(
    workspace: &str,
    candidate_hash: &str,
    note: Option<&str>,
) -> Result<(), String> {
    verify_auto_remember_candidate(workspace, candidate_hash, note)?;
    println!("Auto-remember candidate marked operator_verified.");
    Ok(())
}

pub(crate) fn cmd_auto_remember_reject(
    workspace: &str,
    candidate_hash: &str,
    note: Option<&str>,
) -> Result<(), String> {
    reject_auto_remember_candidate(workspace, candidate_hash, note)?;
    println!("Auto-remember candidate marked operator_rejected.");
    Ok(())
}

pub(crate) fn cmd_auto_remember_requeue(
    workspace: &str,
    candidate_hash: &str,
) -> Result<(), String> {
    requeue_auto_remember_candidate(workspace, candidate_hash)?;
    println!("Auto-remember candidate queued for retry drain.");
    Ok(())
}

fn audit_detail_json(record: &AutoRememberAuditDetail) -> serde_json::Value {
    json!({
        "turn_id": record.turn_id,
        "workspace": record.workspace,
        "candidate_hash": record.candidate_hash,
        "signal_kind": record.signal_kind,
        "stage": record.stage.as_str(),
        "status": record.status.as_str(),
        "retry_count": record.retry_count,
        "error": record.error,
        "raw_content": record.raw_content,
        "entity_key": record.entity_key,
        "supporting_evidence": record.supporting_evidence,
        "created_at": record.created_at,
    })
}

fn truncate_hash(value: &str) -> &str {
    value.get(..12).unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::services::discord::runtime_store::lock_test_env;

    fn with_temp_root<F>(f: F)
    where
        F: FnOnce(&TempDir),
    {
        let _guard = lock_test_env();
        let temp = TempDir::new().unwrap();
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        f(&temp);
        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }

    #[test]
    fn audit_and_summary_commands_accept_existing_sidecar_filters() {
        with_temp_root(|_| {
            let store = AutoRememberStore::open().unwrap();

            store
                .upsert_audit(crate::services::memory::AutoRememberAuditEntry {
                    turn_id: "turn-1",
                    candidate_hash: "hash-validation",
                    signal_kind: "technical_decision",
                    workspace: "agentdesk-default",
                    stage: AutoRememberStage::Validate,
                    status: AutoRememberMemoryStatus::ValidationSkipped,
                    retry_count: 0,
                    error: Some("uncertain"),
                    raw_content: Some("AgentDesk might switch stores later."),
                    entity_key: None,
                    supporting_evidence: None,
                })
                .unwrap();
            store
                .upsert_audit(crate::services::memory::AutoRememberAuditEntry {
                    turn_id: "turn-2",
                    candidate_hash: "hash-retry",
                    signal_kind: "technical_decision",
                    workspace: "agentdesk-default",
                    stage: AutoRememberStage::Remember,
                    status: AutoRememberMemoryStatus::RememberFailed,
                    retry_count: 1,
                    error: Some("temporary failure"),
                    raw_content: Some("AgentDesk standardized on SQLite sidecar."),
                    entity_key: None,
                    supporting_evidence: None,
                })
                .unwrap();

            cmd_auto_remember_audit(
                Some("agentdesk-default"),
                Some(AutoRememberMemoryStatus::RememberFailed),
                Some(AutoRememberStage::Remember),
                Some("technical_decision"),
                None,
                true,
                10,
                true,
            )
            .unwrap();
            cmd_auto_remember_summary(Some("agentdesk-default"), true).unwrap();
        });
    }

    #[test]
    fn audit_command_errors_without_sidecar() {
        with_temp_root(|_| {
            let error =
                cmd_auto_remember_audit(None, None, None, None, None, false, 5, true).unwrap_err();
            assert!(error.contains("sidecar does not exist"));
        });
    }
}
