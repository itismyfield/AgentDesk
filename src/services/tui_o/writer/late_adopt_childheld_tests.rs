//! A pre-deployment thread keeps its running Legacy turn after a refused late handoff.
use super::*;
use crate::services::session_host::surface_policy::VerifiedSurface;
use crate::services::tui_o::channel_policy;

pub(super) fn child_boot(
    f: &Fixture,
) -> (
    test_override::ChannelsGuard,
    crate::services::tui_o::channel_policy::Candidate,
) {
    let mut config = super::super::late_host::config(false);
    const PARENT: u64 = 8_118_021;
    config = serde_json::from_value({
        let mut value = serde_json::to_value(config).unwrap();
        value["tui_o"]["writer"]["channels"] = serde_json::json!([PARENT]);
        value["agents"][0]["channels"]["claude"]["id"] = PARENT.to_string().into();
        value
    })
    .unwrap();
    let boot = BootChannels::validate(&config)
        .unwrap()
        .seeded(&config, |channels, _| {
            Ok(channel_policy::stored(
                Some(f.harness._runtime.path()),
                channels,
            ))
        })
        .unwrap();
    boot.reserve_boot_children(&[(CHANNEL, PARENT)]);
    let thread = serde_json::from_value(serde_json::json!({
        "id": CHANNEL.to_string(), "parent_id": PARENT.to_string(), "type": 11, "name": "fixture"
    }))
    .unwrap();
    assert!(boot.resolve_child(
        CHANNEL,
        VerifiedSurface::from_discord(&thread, ClaudeTui).unwrap(),
        false
    ));
    let candidate = boot.output_candidate(CHANNEL).unwrap();
    crate::config_live_reload::install(config);
    (test_override::force_boot(boot), candidate)
}

#[tokio::test(start_paused = true)]
async fn childheld_running_legacy_thread_posts_once_parks_and_retries_at_the_boundary() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::childheld_running_legacy_thread_posts_once_parks_and_retries_at_the_boundary"
    )) {
        return;
    }
    let f = Fixture::new(false);
    let (_boot, candidate) = child_boot(&f);
    let first = source_id_for("prior", &f.path).unwrap();
    let next = f.harness._runtime.path().join("next.jsonl");
    std::fs::write(&next, row("running", "current answer")).unwrap();
    let source = source_id_for("current", &next).unwrap();
    let legacy = Arc::new(Legacy {
        path: next.clone(),
        frontier: AtomicU64::new(0),
    });
    *f.io.base.legacy.lock().unwrap() = Some(legacy.clone());
    *f.io.base.custody.lock().unwrap() = Ok(Custody::Row);
    f.io.base.busy.store(true, Ordering::SeqCst);
    // Both binds preceded the first adoption read, as in a busy session restored after deploy.
    f.io.log
        .replace(vec![bound(1, first), bound(2, source), audit(3)]);
    let mut handles = start_managed(
        ShadowProvider::Claude,
        true,
        cutover::boot_ownership()
            .into_iter()
            .filter(|row| row.0 == CHANNEL)
            .collect(),
        || HostParts {
            io: f.io.clone(),
            runtime_root: Some(root(&f.harness)),
            gate: f.harness.gate.clone(),
            readiness: f.ready.clone(),
        },
    );
    assert_eq!(handles.len(), 1);
    let handle = handles.pop().unwrap();
    spin().await;
    f.io.base.busy.store(false, Ordering::SeqCst);
    for _ in 0..12 {
        tokio::time::advance(Duration::from_secs(5)).await;
        spin().await;
    }
    assert_eq!(candidate.peek(), Adoption::Released);
    let sent = claim_then_send(Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))), || async {
        f.old.lock().unwrap().push("current answer".into());
    })
    .await;
    assert_eq!(
        sent,
        Ok(BodySend::Sent(())),
        "the existing turn has a decided Legacy owner"
    );
    assert_eq!(*f.old.lock().unwrap(), ["current answer"]);
    assert!(
        f.harness.port.posts().is_empty(),
        "O never claims the running turn"
    );
    assert!(
        cutover::boot_ownership()
            .into_iter()
            .find(|row| row.0 == CHANNEL)
            .unwrap()
            .2
            .is_none()
    );
    let generation = f.ready.hosted_by(CHANNEL);
    let again = start_managed(
        ShadowProvider::Claude,
        true,
        vec![(CHANNEL, Some(ClaudeTui), Some(candidate.clone()))],
        || HostParts {
            io: f.io.clone(),
            runtime_root: Some(root(&f.harness)),
            gate: f.harness.gate.clone(),
            readiness: f.ready.clone(),
        },
    );
    assert!(
        again.is_empty(),
        "even a stale candidate cannot claim readiness while parked"
    );
    assert_eq!(f.ready.hosted_by(CHANNEL), generation);
    append(&next, &closed());
    legacy
        .frontier
        .store(std::fs::metadata(&next).unwrap().len(), Ordering::SeqCst);
    for _ in 0..16 {
        tokio::time::advance(Duration::from_secs(5)).await;
        spin().await;
        if f.ready.accepts(CHANNEL) {
            break;
        }
    }
    assert!(
        f.ready.accepts(CHANNEL),
        "a closed, settled Legacy turn retries O in the same host"
    );
    assert_eq!(candidate.peek(), Adoption::Committed);
    append(&next, &row("after-boundary", "new O answer"));
    polls(3).await;
    assert_eq!(f.harness.port.posts(), ["new O answer"]);
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn childheld_readiness_rechecks_a_candidate_released_during_host_preparation() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::childheld_readiness_rechecks_a_candidate_released_during_host_preparation"
    )) {
        return;
    }
    let f = Fixture::new(false);
    let (_boot, candidate) = child_boot(&f);
    let stale = candidate.clone();
    let handles = start_managed(
        ShadowProvider::Claude,
        true,
        vec![(CHANNEL, Some(ClaudeTui), Some(candidate))],
        || {
            assert!(stale.release_child_run(
                CHANNEL,
                channel_policy::LegacyRun {
                    binding: bound(2, source_id_for("running", &f.path).unwrap()),
                    turn: None,
                    observed_end: std::fs::metadata(&f.path).unwrap().len(),
                }
            ));
            HostParts {
                io: f.io.clone(),
                runtime_root: Some(root(&f.harness)),
                gate: f.harness.gate.clone(),
                readiness: f.ready.clone(),
            }
        },
    );
    assert!(handles.is_empty());
    assert!(!f.ready.is_hosted(CHANNEL));
}
#[tokio::test(start_paused = true)]
async fn childheld_restart_without_released_state_adopts_the_settled_legacy_source() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::childheld_restart_without_released_state_adopts_the_settled_legacy_source"
    )) {
        return;
    }
    let f = Fixture::new(false);
    let (_boot, candidate) = child_boot(&f);
    append(&f.path, &row("old", "already delivered"));
    append(&f.path, &closed());
    let end = std::fs::metadata(&f.path).unwrap().len();
    f.legacy.frontier.store(end, Ordering::SeqCst);
    f.io.log
        .replace(vec![bound(1, source_id_for("fixture", &f.path).unwrap())]);
    let mut handles = start_managed(
        ShadowProvider::Claude,
        true,
        cutover::boot_ownership()
            .into_iter()
            .filter(|row| row.0 == CHANNEL)
            .collect(),
        || HostParts {
            io: f.io.clone(),
            runtime_root: Some(root(&f.harness)),
            gate: f.harness.gate.clone(),
            readiness: f.ready.clone(),
        },
    );
    assert_eq!(handles.len(), 1);
    for _ in 0..16 {
        tokio::time::advance(Duration::from_secs(5)).await;
        spin().await;
        if f.ready.accepts(CHANNEL) {
            break;
        }
    }
    assert!(f.ready.accepts(CHANNEL));
    assert_eq!(candidate.peek(), Adoption::Committed);
    assert!(
        f.harness.port.posts().is_empty(),
        "restart does not replay the previous turn"
    );
    handles.pop().unwrap().stop_and_join().await.unwrap();
}
