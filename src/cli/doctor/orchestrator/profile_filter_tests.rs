use super::{
    Check, CheckGroup, DoctorOptions, HealthSnapshot, build_json_report, check_group_from_report,
    check_mailbox_consistency, checksum_mismatch_evidence, doctor_profile_includes_check,
    format_checksum_mismatches,
};
use crate::cli::doctor::contract::{DoctorProfile, RunContext};
use serde_json::json;

#[cfg(not(target_os = "macos"))]
#[test]
fn non_macos_fd_probes_are_empty() {
    assert!(super::tmux_server_pids().is_empty());
    assert!(super::check_file_descriptor_headroom().is_none());
}

#[test]
fn mailbox_consistency_findings_use_provider_runtime_group() {
    let snapshot = HealthSnapshot {
        base: "http://localhost:8787".to_string(),
        body: Some(json!({
            "global_active": 0,
            "mailboxes": [{
                "channel_id": 42,
                "has_cancel_token": true,
                "queue_depth": 0,
                "watcher_attached": false,
                "inflight_state_present": false,
                "tmux_present": false,
                "process_present": false,
                "active_dispatch_present": false
            }]
        })),
        error: None,
    };

    let checks = check_mailbox_consistency(&snapshot);

    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].id, "mailbox_busy_without_active_turn");
    assert_eq!(checks[0].fix_safety, super::FixSafety::SafeLocalRepair);
    assert_eq!(checks[0].group, CheckGroup::ProviderRuntime);
    assert_eq!(checks[0].subsystem, "provider_runtime");
}

#[test]
fn quick_profile_keeps_dispatch_outbox_checks() {
    let dispatch_outbox = Check::ok(
        "dispatch_outbox",
        CheckGroup::Core,
        "Dispatch Outbox",
        "dispatch outbox is healthy",
    )
    .with_subsystem("dispatch_outbox");
    let config_audit = Check::ok(
        "config_paths",
        CheckGroup::Core,
        "Config Paths",
        "config paths are healthy",
    )
    .with_subsystem("config_audit");

    assert!(doctor_profile_includes_check(
        DoctorProfile::Quick,
        &dispatch_outbox
    ));
    assert!(!doctor_profile_includes_check(
        DoctorProfile::Quick,
        &config_audit
    ));
}

#[test]
fn quick_profile_keeps_portable_optional_state_checks() {
    let optional_connector = Check::ok(
        "optional_obsidian_agents",
        CheckGroup::OptionalConnectors,
        "Obsidian agent prompts",
        "state=missing_config reason=missing_config",
    );

    assert!(doctor_profile_includes_check(
        DoctorProfile::Quick,
        &optional_connector
    ));
}

#[test]
fn json_report_exposes_portable_readiness_sections() {
    let checks = vec![
        Check::ok("runtime_root", CheckGroup::Core, "Runtime root", "ok").with_subsystem("config"),
        Check::ok(
            "launchd_service",
            CheckGroup::Core,
            "Launchd service",
            "launchd not loaded in manual mode",
        )
        .with_subsystem("launchd"),
        Check::ok(
            "optional_obsidian_agents",
            CheckGroup::OptionalConnectors,
            "Obsidian agent prompts",
            "state=missing_config reason=missing_config",
        ),
    ];
    let options = DoctorOptions {
        fix: false,
        json: true,
        allow_restart: false,
        repair_sqlite_cache: false,
        allow_remote: false,
        profile: None,
        run_context: RunContext::ManualCli,
        artifact_path: None,
    };
    let report = build_json_report(&options, &checks, &[]);

    for section in ["core", "config", "launchd", "optional_connectors"] {
        assert!(
            report.sections.contains_key(section),
            "missing doctor section {section}"
        );
    }
    assert_eq!(report.sections["optional_connectors"].summary.total, 1);
}

#[test]
fn report_group_round_trip_keeps_portable_groups() {
    assert_eq!(
        check_group_from_report("optional_connectors"),
        CheckGroup::OptionalConnectors
    );
}

#[test]
fn postgres_checksum_mismatch_detail_includes_applied_and_resolved_hashes() {
    let mismatches = vec![crate::db::postgres::MigrationChecksumMismatch {
        version: 1,
        applied_checksum: "oldchecksum".to_string(),
        resolved_checksum: "newchecksum".to_string(),
    }];

    let formatted = format_checksum_mismatches(&mismatches);
    assert!(formatted.contains("version=1"));
    assert!(formatted.contains("applied_checksum=oldchecksum"));
    assert!(formatted.contains("resolved_checksum=newchecksum"));

    let evidence = checksum_mismatch_evidence(&mismatches);
    assert_eq!(evidence[0]["version"], 1);
    assert_eq!(evidence[0]["applied_checksum"], "oldchecksum");
    assert_eq!(evidence[0]["resolved_checksum"], "newchecksum");
}
