use super::spawn_skill_manifest_audit_once;
use crate::server::routes::skills_manifest_audit::{
    audit_skill_manifest_agents, build_manifest_audit_request,
};
use std::collections::BTreeSet;
use std::sync::{
    Arc, Once,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn stale_agent_and_unlisted_skill_emit_one_boot_warning_in_same_process() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = temp.path().join("manifest.json");
    std::fs::write(
        &manifest,
        r#"{"skills":{"installed-later":{"agents":["ghost-agent"]}}}"#,
    )
    .unwrap();
    let skill = temp.path().join("missing-skill");
    std::fs::create_dir(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "skill").unwrap();
    let once = Once::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for _ in 0..16 {
        let manifest = manifest.clone();
        let calls = calls.clone();
        let tx = tx.clone();
        spawn_skill_manifest_audit_once(
            &once,
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let ids: BTreeSet<String> = ["live".to_string()].into();
                audit_skill_manifest_agents(build_manifest_audit_request(
                    ids.clone(),
                    Some(ids.clone()),
                    Some(ids),
                    vec![(
                        manifest,
                        crate::server::routes::skills_manifest_audit::DirectoryScope::OwnManifest,
                    )],
                ))
                .warning()
            },
            move |message| {
                tx.send(message).unwrap();
            },
        );
    }
    drop(tx);
    let message = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(message.contains("ghost-agent"));
    // The warning embeds paths as JSON strings, which escape Windows separators.
    assert!(message.contains(&serde_json::json!(skill.display().to_string()).to_string()));
    assert_eq!(rx.recv().await, None);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn clean_audit_emits_no_boot_warning_and_scheduling_never_waits_for_inspection() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = temp.path().join("manifest.json");
    std::fs::write(
        &manifest,
        r#"{"skills":{"not-installed":{"agents":["live"]}}}"#,
    )
    .unwrap();
    let once = Once::new();
    let (release, held) = tokio::sync::oneshot::channel();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    spawn_skill_manifest_audit_once(
        &once,
        async move {
            held.await.unwrap();
            let ids: BTreeSet<String> = ["live".to_string()].into();
            audit_skill_manifest_agents(build_manifest_audit_request(
                ids.clone(),
                Some(ids.clone()),
                Some(ids),
                vec![(
                    manifest,
                    crate::server::routes::skills_manifest_audit::DirectoryScope::OwnManifest,
                )],
            ))
            .warning()
        },
        move |message| {
            tx.send(message).unwrap();
        },
    );
    release.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap(),
        None
    );
}
