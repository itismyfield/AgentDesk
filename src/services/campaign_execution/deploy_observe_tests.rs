use chrono::TimeDelta;
use serde_json::json;

use super::*;

fn deployment() -> DeploymentWatch {
    let now = Utc::now();
    let targets = ["mac-mini", "mac-peer"]
        .map(|host| DeploymentExpectation {
            action_id: "deployment".into(),
            host: host.into(),
            expected_sha: "a".repeat(40),
            expected_binary_digest: None,
            previous_incarnation: "boot-before".into(),
        })
        .to_vec();
    DeploymentWatch {
        action_id: "deployment".into(),
        not_before: now - TimeDelta::seconds(10),
        deadline: now + TimeDelta::minutes(3),
        targets,
    }
}

fn body(target: &DeploymentExpectation, observed_at: DateTime<Utc>, started_at_ms: i64) -> Value {
    json!({
        "boot_incarnation": "boot-after",
        "release_source": {
            "node_hostname": target.host,
            "deployed_repo_head": target.expected_sha,
        },
        "running_binary": {
            "source_sha": target.expected_sha,
            "target": "aarch64-apple-darwin",
            "observed_at": observed_at,
            "boot_started_at_ms": started_at_ms,
        }
    })
}

#[test]
fn m9_only_the_running_source_can_confirm_a_deployment() {
    let watch = deployment();
    let mut target = watch.targets[0].clone();
    let requested = Utc::now();
    let observed = requested + TimeDelta::milliseconds(5);
    let received = observed + TimeDelta::milliseconds(1);
    let mut health = body(&target, observed, requested.timestamp_millis());
    assert!(verify_sample(&target, &health, watch.not_before, requested, received).is_some());
    health["running_binary"]["source_sha"] = json!("b".repeat(40));
    assert!(verify_sample(&target, &health, watch.not_before, requested, received).is_none());
    health["running_binary"]["source_sha"] = json!(target.expected_sha);
    target.expected_binary_digest = Some("sha256:previous-artifact".into());
    assert!(verify_sample(&target, &health, watch.not_before, requested, received).is_none());
    target.expected_binary_digest = None;
    health.as_object_mut().unwrap().remove("running_binary");
    assert!(verify_sample(&target, &health, watch.not_before, requested, received).is_none());
}

#[test]
fn m9_cached_health_at_or_before_the_query_is_insufficient() {
    let watch = deployment();
    let target = &watch.targets[0];
    let requested = Utc::now();
    let received = requested + TimeDelta::milliseconds(5);
    for observed in [requested - TimeDelta::milliseconds(1), requested] {
        let health = body(
            target,
            observed,
            (requested - TimeDelta::seconds(1)).timestamp_millis(),
        );
        assert!(verify_sample(target, &health, watch.not_before, requested, received).is_none());
    }
    let future = body(
        target,
        received + TimeDelta::milliseconds(1),
        requested.timestamp_millis(),
    );
    assert!(verify_sample(target, &future, watch.not_before, requested, received).is_none());
}

#[test]
fn m9_boot_must_have_started_after_the_bound_deployment_time() {
    let watch = deployment();
    let target = &watch.targets[0];
    let requested = Utc::now();
    let observed = requested + TimeDelta::milliseconds(1);
    let old = body(target, observed, watch.not_before.timestamp_millis());
    assert!(verify_sample(target, &old, watch.not_before, requested, observed).is_none());
    let mut matching = body(target, observed, requested.timestamp_millis());
    matching["boot_incarnation"] = json!(target.previous_incarnation);
    assert!(verify_sample(target, &matching, watch.not_before, requested, observed).is_none());
}

#[test]
fn m9_deadline_precedes_confirmation_even_with_all_hosts() {
    let watch = deployment();
    let requested = Utc::now();
    let observed = requested + TimeDelta::milliseconds(10);
    let evidence = || {
        watch
            .targets
            .iter()
            .map(|target| {
                verify_sample(
                    target,
                    &body(target, observed, requested.timestamp_millis()),
                    watch.not_before,
                    requested,
                    observed,
                )
            })
            .collect()
    };
    assert!(matches!(settle(&watch, evidence(), observed),
        DeploymentProgress::Confirmed { witnesses } if witnesses.len() == 2));
    assert!(matches!(
        settle(&watch, evidence(), watch.deadline),
        DeploymentProgress::Failed { .. }
    ));
    assert_eq!(
        settle(
            &watch,
            vec![evidence().into_iter().next().unwrap()],
            observed
        ),
        DeploymentProgress::Waiting {
            missing_hosts: vec!["mac-peer".into()]
        }
    );
}

#[cfg(unix)]
#[tokio::test]
async fn m9_the_worker_reobserves_every_host_after_an_indeterminate_restart_pg() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::db::server_campaigns::{
        CommandOutcome, ReceiptResponse,
        command_transaction::{self, Preparation, StoreRequest},
    };
    use crate::domain::campaign::model::*;
    use crate::services::campaign_execution::{
        adk_resources::AdkResources,
        commands,
        control::tests as domain,
        effect_envelope::execution_nonce,
        host_gate::{
            EffectAuthority, EffectEnvelope, EffectGate, HostNodeFence, Initiation, RunFence,
        },
    };

    struct Probe {
        watch: DeploymentWatch,
        cached: Mutex<Option<Value>>,
        polls: AtomicUsize,
    }
    impl DeploymentHealth for Probe {
        fn detail<'a>(&'a self, host: &'a str) -> HealthReply<'a> {
            Box::pin(async move {
                self.polls.fetch_add(1, Ordering::SeqCst);
                if host == "mac-peer"
                    && let Some(value) = self.cached.lock().unwrap().as_ref().cloned()
                {
                    return Ok(value);
                }
                tokio::task::yield_now().await;
                let target = self
                    .watch
                    .targets
                    .iter()
                    .find(|target| target.host == host)
                    .unwrap();
                Ok(body(
                    target,
                    Utc::now(),
                    (self.watch.not_before + TimeDelta::seconds(1)).timestamp_millis(),
                ))
            })
        }
    }

    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let hostname = std::process::Command::new("/bin/hostname")
        .output()
        .unwrap();
    let hostname = String::from_utf8(hostname.stdout).unwrap();
    let open = || {
        HostWorker::open(
            root,
            "campaign",
            "a",
            hostname.trim(),
            "campaign",
            AdkResources::new(root, root).unwrap(),
            Utc::now(),
        )
        .unwrap()
    };
    let initial = open();
    let host = initial.identity().clone();
    drop(initial);
    let watch = deployment();
    let mut expired = watch.clone();
    expired.action_id = "expired".into();
    expired.deadline = Utc::now() - TimeDelta::seconds(1);
    for target in &mut expired.targets {
        target.action_id = expired.action_id.clone();
    }

    let render = |outcome: &CommandOutcome| {
        Ok(ReceiptResponse {
            status: 200,
            body: serde_json::to_value(outcome)?,
        })
    };
    let mut state = domain::running();
    commands::create(
        &pool,
        &state.id,
        &domain::input(),
        "create",
        Utc::now(),
        render,
    )
    .await
    .unwrap();
    let tx = pool.begin().await.unwrap();
    let Preparation::Fresh(prepared) = command_transaction::prepare(
        tx,
        &state.id,
        "seed-deployment",
        Utc::now(),
        StoreRequest::Command(Command::ConfirmStopped),
    )
    .await
    .unwrap() else {
        panic!("fixture command already existed")
    };
    state.event_sequence = prepared.previous().unwrap().event_sequence + 1;
    let binding = domain::binding(&state);
    let run = state.run.as_mut().unwrap();
    let hash = digest_of(&binding).unwrap();
    let path = root.join(blake3::hash(b"campaign\na").to_hex().as_str());
    let now = Utc::now();
    let mut journal = EffectGate::recover_journal(&path, host.clone(), now).unwrap();
    journal
        .apply_run_fence(RunFence {
            run_id: run.id.clone(),
            control_version: run.control_version,
            stop: None,
        })
        .unwrap();
    journal
        .apply_node_fence(HostNodeFence {
            run_id: run.id.clone(),
            node_id: "a".into(),
            epoch: 0,
            interrupted: false,
        })
        .unwrap();
    for item in [&watch, &expired] {
        run.actions.insert(
            item.action_id.clone(),
            Action {
                id: item.action_id.clone(),
                kind: ActionKind::Deploy,
                state: ActionState::StartPending,
                binding: binding.clone(),
                target: host.clone(),
                initiation_receipt: None,
                reconciliation: None,
            },
        );
        let payload = item.payload_digest().unwrap();
        let effect = EffectEnvelope {
            action_id: item.action_id.clone(),
            kind: ActionKind::Deploy,
            run_id: run.id.clone(),
            node_id: "a".into(),
            origin_attempt_id: binding.attempt_id.clone(),
            origin_binding_digest: hash.clone(),
            target_host_gate: host.clone(),
            execution_nonce: execution_nonce(&item.action_id, &payload),
            payload_digest: payload,
            phase: None,
            authority: EffectAuthority::CurrentAttempt {
                attempt_id: binding.attempt_id.clone(),
                binding_digest: hash.clone(),
                node_epoch: 0,
                run_control_version: run.control_version,
            },
        };
        let reply = journal
            .admit_and_initiate::<()>(&effect, now, |_| {
                Err(std::io::Error::other("lost start reply"))
            })
            .unwrap();
        assert!(matches!(reply.initiation, Initiation::Indeterminate { .. }));
    }
    drop(journal);
    let decision = Decision {
        events: vec![Event {
            sequence: state.event_sequence,
            run_id: Some(run.id.clone()),
            command_id: "seed-deployment".into(),
            observed_at: now,
            kind: EventKind::RunStarted,
        }],
        result: CommandResult {
            definition_revision: state.definition_revision,
            run_id: Some(run.id.clone()),
            control_version: Some(run.control_version),
            event_sequence: state.event_sequence,
            control: state.control,
        },
        state,
        records: Vec::new(),
    };
    let staged = prepared.stage(Some(Ok(decision))).await.unwrap();
    let response = render(staged.outcome()).unwrap();
    let (tx, _) = staged.record_receipt(response).await.unwrap();
    tx.commit().await.unwrap();

    let probe = Probe {
        watch: watch.clone(),
        cached: Mutex::new(None),
        polls: AtomicUsize::new(0),
    };
    let mut worker = open();
    let timeout = Duration::from_secs(1);
    let mut narrower = watch.clone();
    narrower.targets.pop();
    assert!(matches!(
        step_deployment(&mut worker, &pool, &narrower, &probe, timeout).await,
        Err(WorkerError::Binding)
    ));
    let DeploymentProgress::Confirmed { witnesses } =
        step_deployment(&mut worker, &pool, &watch, &probe, timeout)
            .await
            .unwrap()
    else {
        panic!("both fresh witnesses are required")
    };
    assert_eq!(witnesses.len(), 2);
    let peer = &witnesses[1];
    *probe.cached.lock().unwrap() = Some(body(
        &watch.targets[1],
        peer.observed_at,
        peer.boot_started_at_ms,
    ));
    drop(worker);

    let mut worker = open();
    assert_eq!(
        step_deployment(&mut worker, &pool, &watch, &probe, timeout)
            .await
            .unwrap(),
        DeploymentProgress::Waiting {
            missing_hosts: vec!["mac-peer".into()]
        }
    );
    assert_eq!(probe.polls.load(Ordering::SeqCst), 4);
    *probe.cached.lock().unwrap() = None;
    assert!(
        matches!(step_deployment(&mut worker, &pool, &watch, &probe, timeout).await.unwrap(),
        DeploymentProgress::Confirmed { witnesses } if witnesses.len() == 2)
    );
    let failure = step_deployment(&mut worker, &pool, &expired, &probe, timeout)
        .await
        .unwrap();
    assert!(matches!(failure, DeploymentProgress::Failed { .. }));
    drop(worker);
    let mut worker = open();
    assert_eq!(
        step_deployment(&mut worker, &pool, &expired, &probe, timeout)
            .await
            .unwrap(),
        failure
    );
    assert_eq!(probe.polls.load(Ordering::SeqCst), 6);
    assert_eq!(worker.effect_entry(&watch.action_id).unwrap().attempts, 1);
    pool.close().await;
    db.drop().await;
}
