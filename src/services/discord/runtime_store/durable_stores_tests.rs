//! The force-clear report, delivery record and watcher owner context flush nothing past a failed
//! publication, flush the new bytes with their chain, and keep a save whose later flush fails.

use super::*;
use crate::services::discord::outbound::delivery_record;
use crate::services::discord::recovery_paths::restart::{
    RecoveryForceClearReport, persist_force_clear_report_in_root,
};

fn report(channel_id: u64) -> RecoveryForceClearReport {
    RecoveryForceClearReport {
        version: 1,
        provider: "codex".into(),
        channel_id,
        user_msg_id: 1,
        current_msg_id: 2,
        session_key: None,
        dispatch_id: None,
        tmux_session_name: None,
        branch: "missing_tmux".into(),
        reason_code: "durable_test".into(),
        attempts: 1,
        budget: 3,
        started_at: "2026-10-22 00:00:00".into(),
        cleared_at: "2026-10-22 00:00:01".into(),
        full_response: "the only copy of the response".into(),
    }
}

fn fail(steps: Vec<Step>) -> Plan {
    Plan {
        fail: steps,
        ..Plan::default()
    }
}

/// A force-clear report is the sole copy of a cleared response: a failed write publishes and
/// flushes nothing, a written one is flushed with its chain, and a later flush failure is `Ok`.
#[test]
fn force_clear_report_is_published_durably() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp
        .path()
        .join("runtime")
        .join("discord_recovery_force_clear");
    {
        let watch = Watch::new(temp.path(), fail(vec![Step::FileSync]));
        persist_force_clear_report_in_root(&root, &report(7_234_800)).unwrap_err();
        let flushed = watch.count(Step::ParentSync) + watch.count(Step::Ancestors);
        assert_eq!(flushed, 0, "nothing flushed for an unpublished report");
    }
    let watch = Watch::new(temp.path(), Plan::default());
    let path = persist_force_clear_report_in_root(&root, &report(7_234_801)).unwrap();
    let bytes = fs::read_to_string(&path).unwrap();
    assert_eq!(
        flushes_of(&watch, &path, Step::ParentSync),
        vec![(true, Some(bytes))]
    );
    let ancestors = watch.flushed(Step::Ancestors);
    assert!(ancestors.contains(&root) && ancestors.contains(&temp.path().join("runtime")));
    drop(watch);

    let _failing = Watch::new(temp.path(), fail(vec![Step::ParentSync]));
    let path = persist_force_clear_report_in_root(&root, &report(7_234_802)).unwrap();
    assert!(fs::read_to_string(path).unwrap().contains("the only copy"));
}

/// A confirmed send's delivery record is not published past a failed rename, is flushed once
/// published, and stays (the body still reads as delivered) when the later flush fails.
#[test]
fn delivery_record_publication_keeps_delivered_content() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let env = RootEnv::new();
    let provider = ProviderKind::Codex;
    let tmux = "AgentDesk-codex-durable-7234";
    let channel = |id: u64| ChannelId::new(id);
    let record = |id: u64| {
        env.runtime()
            .join("discord_delivery_records/codex")
            .join(format!("{id}.json"))
    };
    let record_body = |id: u64| {
        delivery_record::record_delivered_content_fingerprint(&provider, channel(id), tmux, "body")
    };
    let watch = Watch::new(env.root(), Plan::default());

    let armed = rename_fault::Armed::new(&record(7_234_810));
    record_body(7_234_810);
    assert!(!armed.evidence().1.is_empty(), "a real rename failure");
    assert!(!record(7_234_810).exists());
    assert!(flushes_of(&watch, &record(7_234_810), Step::ParentSync).is_empty());
    drop(armed);

    record_body(7_234_811);
    let bytes = fs::read_to_string(record(7_234_811)).unwrap();
    assert_eq!(
        flushes_of(&watch, &record(7_234_811), Step::ParentSync),
        vec![(true, Some(bytes))]
    );
    drop(watch);

    let _failing = Watch::new(env.root(), fail(vec![Step::ParentSync, Step::Ancestors]));
    record_body(7_234_812);
    let seen = delivery_record::recent_delivered_content_matches(
        &provider,
        channel(7_234_812),
        tmux,
        "body",
    );
    assert!(
        seen,
        "the delivered body is not forgotten after a flush failure"
    );
}

/// The watcher owner context is published durably and a flush failure after its rename is
/// reported as written.
#[test]
fn watcher_owner_context_is_published_durably() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let env = RootEnv::new();
    let provider = ProviderKind::Codex;
    let context = |id: u64| {
        env.runtime()
            .join("discord_delivery_owner_context/codex")
            .join(format!("{id}.json"))
    };
    let write = |id: u64| {
        delivery_record::record_watcher_owner_channel_context(
            &provider,
            ChannelId::new(id),
            ChannelId::new(9),
            "AgentDesk-codex-durable-owner",
        )
    };
    let watch = Watch::new(env.root(), Plan::default());
    let armed = rename_fault::Armed::new(&context(7_234_820));
    write(7_234_820).unwrap_err();
    assert!(!context(7_234_820).exists());
    assert!(flushes_of(&watch, &context(7_234_820), Step::ParentSync).is_empty());
    drop(armed);

    write(7_234_821).unwrap();
    let bytes = fs::read_to_string(context(7_234_821)).unwrap();
    assert_eq!(
        flushes_of(&watch, &context(7_234_821), Step::ParentSync),
        vec![(true, Some(bytes))]
    );
    drop(watch);

    let _failing = Watch::new(env.root(), fail(vec![Step::ParentSync]));
    write(7_234_822).unwrap();
    assert!(context(7_234_822).exists());
}
