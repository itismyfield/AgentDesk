use super::*;
use crate::services::discord::boot_retirement::BootCohort;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc,
};

fn bot(id: &str, provider: &str, utility: bool) -> BootBot {
    BootBot {
        slot: id.into(),
        provider: provider.into(),
        utility,
        selection: BootSelection {
            runtime_kind: "codex_tui".into(),
            turn_channels: [7].into(),
        },
    }
}

fn cohort(bots: Vec<BootBot>) -> Arc<BootCohort<usize>> {
    BootCohort::<usize>::install_in(&OnceLock::new(), BootRoster::new(bots).unwrap()).unwrap()
}

async fn reaped(slot: &BootSlot<usize>) -> Arc<Completed<usize>> {
    let done = slot.work_once().unwrap().run_once(|| 1).await.unwrap();
    slot.arrive_reaped(&done).unwrap();
    done
}

fn recording(
    c: &Arc<BootCohort<usize>>,
    calls: &Arc<AtomicUsize>,
) -> impl FnMut(&str, &mut BootPublication) -> Result<(), BootWorkFailure> + Send + 'static {
    let epoch = c.epoch();
    let calls = calls.clone();
    move |provider, publication| {
        calls.fetch_add(1, Ordering::SeqCst);
        publication.publish_with(epoch, provider, 7, || Ok(()))
    }
}

#[tokio::test]
async fn all_bots_reap_before_any_confirmation() {
    let c = cohort(vec![
        bot("first", "Codex", false),
        bot("second", "codex", false),
    ]);
    let first = BootSlot::begin(&c, "first").unwrap();
    let second = BootSlot::begin(&c, "second").unwrap();
    let done = reaped(&first).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let started = c.try_start_confirmation(recording(&c, &calls));
    if started {
        c.wait_released().await.unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(c.snapshot().published_keys.is_empty());
    second.arrive_reaped(&done).unwrap();
    assert!(c.try_start_confirmation(recording(&c, &calls)));
    c.wait_released().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn all_reaper_workers_finish_before_confirmation() {
    let c = cohort(vec![
        bot("first", "codex", false),
        bot("second", "claude", false),
    ]);
    let first = BootSlot::begin(&c, "first").unwrap();
    let second = Arc::new(BootSlot::begin(&c, "second").unwrap());
    reaped(&first).await;
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let pending = second.clone();
    let job = tokio::spawn(async move {
        let done = pending
            .work_once()
            .unwrap()
            .run_once(move || {
                entered_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
                1
            })
            .await
            .unwrap();
        pending.arrive_reaped(&done).unwrap();
    });
    entered_rx.recv().await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let started = c.try_start_confirmation(recording(&c, &calls));
    if started {
        c.wait_released().await.unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(c.snapshot().published_keys.is_empty());
    resume_tx.send(()).unwrap();
    job.await.unwrap();
    assert!(c.try_start_confirmation(recording(&c, &calls)));
    c.wait_released().await.unwrap();
}

#[tokio::test]
async fn all_confirmations_finish_before_release() {
    let c = cohort(vec![
        bot("first", "claude", false),
        bot("second", "codex", false),
    ]);
    let first = BootSlot::begin(&c, "first").unwrap();
    let second = BootSlot::begin(&c, "second").unwrap();
    reaped(&first).await;
    reaped(&second).await;
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let epoch = c.epoch();
    assert!(c.try_start_confirmation(move |provider, publication| {
        publication.publish_with(epoch, provider, 7, || Ok(()))?;
        if provider == "codex" {
            entered_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
        }
        Ok(())
    }));
    entered_rx.recv().await.unwrap();
    assert_eq!(c.snapshot().completed_providers, ["claude"]);
    assert!(!c.snapshot().supervisors_released);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), c.wait_released())
            .await
            .is_err()
    );
    resume_tx.send(()).unwrap();
    c.wait_released().await.unwrap();
    assert_eq!(c.snapshot().completed_providers, ["claude", "codex"]);
}

#[tokio::test]
async fn same_provider_reaped_and_confirmed_once() {
    let c = cohort(vec![
        bot("first", "codex", false),
        bot("second", "codex", false),
    ]);
    let first = BootSlot::begin(&c, "first").unwrap();
    let second = BootSlot::begin(&c, "second").unwrap();
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let a = first
        .work_once()
        .unwrap()
        .run_once(move || counter.fetch_add(1, Ordering::SeqCst))
        .await
        .unwrap();
    let counter = runs.clone();
    let b = second
        .work_once()
        .unwrap()
        .run_once(move || counter.fetch_add(1, Ordering::SeqCst))
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    first.arrive_reaped(&a).unwrap();
    second.arrive_reaped(&b).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(c.try_start_confirmation(recording(&c, &calls)));
    assert!(!c.try_start_confirmation(recording(&c, &calls)));
    let release = c.wait_released().await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.unwrap();
}

#[tokio::test]
async fn duplicate_slot_does_not_complete_another_slot() {
    let c = cohort(vec![
        bot("first", "codex", false),
        bot("second", "codex", false),
    ]);
    let first = BootSlot::begin(&c, "first").unwrap();
    assert!(BootSlot::begin(&c, "first").is_err());
    let done = reaped(&first).await;
    let duplicate = first.arrive_reaped(&done);
    assert_eq!(c.snapshot().reaped, 1);
    assert!(duplicate.is_err());
    assert_eq!(c.snapshot().waiting_bots, ["second"]);
    assert_eq!(c.snapshot().expected, 2);
}

#[tokio::test]
async fn utility_exclusion_is_explicit() {
    let c = cohort(vec![
        bot("runtime", "codex", false),
        bot("utility", "codex", true),
    ]);
    let runtime = BootSlot::begin(&c, "runtime").unwrap();
    let utility = BootSlot::begin(&c, "utility").unwrap();
    assert!(runtime.exclude_no_runtime().is_err());
    utility.exclude_no_runtime().unwrap();
    assert_eq!(c.snapshot().excluded, 1);
    assert_eq!(c.snapshot().expected, 2);
    reaped(&runtime).await;
    assert!(c.try_start_confirmation(|_, _| Ok(())));
    c.wait_released().await.unwrap();
}

#[tokio::test]
async fn dropped_slot_holds_without_shrinking_roster() {
    for arrived in [false, true] {
        let c = cohort(vec![
            bot("first", "codex", false),
            bot("second", "codex", false),
        ]);
        let first = BootSlot::begin(&c, "first").unwrap();
        if arrived {
            reaped(&first).await;
        }
        drop(first);
        assert_eq!(c.snapshot().expected, 2);
        assert_eq!(c.snapshot().failed, 1);
        assert_eq!(c.snapshot().phase, BootPhase::Held);
        assert!(!c.try_start_confirmation(|_, _| Ok(())));
        assert!(c.wait_released().await.is_err());
    }
}

#[tokio::test(start_paused = true)]
async fn timeout_holds_and_late_completion_can_release() {
    let c = cohort(vec![bot("first", "codex", false)]);
    tokio::time::advance(std::time::Duration::from_secs(120)).await;
    assert!(!c.snapshot().timed_out);
    let slot = Arc::new(BootSlot::begin(&c, "first").unwrap());
    let runs = Arc::new(AtomicUsize::new(0));
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let pending = slot.clone();
    let counter = runs.clone();
    let job = tokio::spawn(async move {
        pending
            .work_once()
            .unwrap()
            .run_once(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                entered_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
                1
            })
            .await
            .unwrap()
    });
    entered_rx.recv().await.unwrap();
    tokio::time::advance(std::time::Duration::from_secs(120)).await;
    tokio::task::yield_now().await;
    assert!(c.snapshot().timed_out);
    assert_eq!(c.snapshot().phase, BootPhase::Held);
    assert_eq!(c.snapshot().waiting_bots, ["first"]);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    tokio::time::resume();
    resume_tx.send(()).unwrap();
    let done = tokio::time::timeout(std::time::Duration::from_secs(5), job)
        .await
        .expect("late Completed must survive timeout")
        .unwrap();
    slot.arrive_reaped(&done).unwrap();
    assert!(c.try_start_confirmation(|_, _| Ok(())));
    c.wait_released().await.unwrap();
    assert!(c.snapshot().timed_out);
    let late = slot.work_once().unwrap().run_once(|| 999).await.unwrap();
    assert!(Arc::ptr_eq(&done, &late));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn receipt_epoch_and_provider_must_match() {
    let a = cohort(vec![bot("first", "codex", false)]);
    let b = cohort(vec![
        bot("first", "codex", false),
        bot("second", "claude", false),
    ]);
    let foreign = BootSlot::begin(&a, "first").unwrap();
    let target = BootSlot::begin(&b, "first").unwrap();
    let other = BootSlot::begin(&b, "second").unwrap();
    let wrong_epoch = foreign.work_once().unwrap().run_once(|| 1).await.unwrap();
    let epoch_result = target.arrive_reaped(&wrong_epoch);
    assert_eq!(b.snapshot().reaped, 0);
    assert!(epoch_result.is_err());
    let wrong_provider = other.work_once().unwrap().run_once(|| 1).await.unwrap();
    let provider_result = target.arrive_reaped(&wrong_provider);
    assert_eq!(b.snapshot().reaped, 0);
    assert!(provider_result.is_err());
    assert_eq!(b.snapshot().waiting_bots.len(), 2);
}

#[tokio::test]
async fn failed_confirmation_keeps_partial_results_held() {
    let c = cohort(vec![
        bot("first", "claude", false),
        bot("second", "codex", false),
    ]);
    let first = BootSlot::begin(&c, "first").unwrap();
    let second = BootSlot::begin(&c, "second").unwrap();
    reaped(&first).await;
    reaped(&second).await;
    let epoch = c.epoch();
    assert!(c.try_start_confirmation(move |provider, publication| {
        if provider == "codex" {
            panic!("confirmation worker failed");
        }
        publication.publish_with(epoch, provider, 7, || Ok(()))
    }));
    let release = c.wait_released().await;
    let h = c.snapshot();
    assert_eq!(h.phase, BootPhase::Held);
    assert_eq!(h.completed_providers, ["claude"]);
    assert_eq!(h.published_keys, [("claude".to_owned(), 7)]);
    assert!(!h.supervisors_released);
    assert!(release.is_err());
}

#[tokio::test]
async fn empty_epoch_is_sealed_without_publication() {
    let mut empty = bot("first", "codex", false);
    empty.selection.turn_channels.clear();
    let cell = OnceLock::new();
    let c = BootCohort::<usize>::install_in(&cell, BootRoster::new(vec![empty]).unwrap()).unwrap();
    c.wait_released().await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(!c.try_start_confirmation(recording(&c, &calls)));
    assert!(c.snapshot().published_keys.is_empty());
    assert!(
        BootCohort::<usize>::install_in(
            &cell,
            BootRoster::new(vec![bot("second", "codex", false)]).unwrap()
        )
        .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn second_process_epoch_is_rejected() {
    const CHILD: &str = "AGENTDESK_BOOT_EPOCH_FIXTURE";
    if std::env::var_os(CHILD).is_none() {
        let name = "services::discord::runtime_bootstrap::boot_retirement::cohort_tests::second_process_epoch_is_rejected";
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let c = BootCohort::<usize>::install_process(
        BootRoster::new(vec![bot("first", "codex", false)]).unwrap(),
    )
    .unwrap();
    let slot = BootSlot::begin(&c, "first").unwrap();
    reaped(&slot).await;
    assert!(c.try_start_confirmation(|_, _| Ok(())));
    c.wait_released().await.unwrap();
    let second = BootCohort::<usize>::install_process(
        BootRoster::new(vec![bot("second", "codex", false)]).unwrap(),
    );
    let effects = Arc::new(AtomicUsize::new(0));
    if let Ok(second) = &second {
        let slot = BootSlot::begin(second, "second").unwrap();
        reaped(&slot).await;
        second.try_start_confirmation(recording(second, &effects));
        second.wait_released().await.unwrap();
    }
    assert_eq!(
        effects.load(Ordering::SeqCst),
        0,
        "sealed process must not issue new publication"
    );
    assert!(second.is_err());
}

#[tokio::test]
async fn snapshot_remains_readable_while_work_is_blocked() {
    let c = cohort(vec![bot("first", "codex", false)]);
    let slot = BootSlot::begin(&c, "first").unwrap();
    reaped(&slot).await;
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    assert!(c.try_start_confirmation(move |_, _| {
        entered_tx.send(()).unwrap();
        resume_rx.recv().unwrap();
        Ok(())
    }));
    entered_rx.recv().await.unwrap();
    let snapshot_cohort = c.clone();
    let snapshot = tokio::task::spawn_blocking(move || {
        serde_json::to_value(snapshot_cohort.snapshot()).unwrap()
    });
    let observed = tokio::time::timeout(std::time::Duration::from_secs(2), snapshot).await;
    resume_tx.send(()).unwrap();
    let health = observed
        .expect("snapshot must finish before callback latch opens")
        .unwrap();
    assert_eq!(health["phase"], "Confirming");
    assert_eq!(health["expected"], 1);
    assert_eq!(health["waiting_providers"], serde_json::json!(["codex"]));
    assert!(!health.to_string().contains("token"));
    c.wait_released().await.unwrap();
}

#[test]
fn roster_rejects_duplicates_and_conflicting_snapshots() {
    assert!(
        BootRoster::new(vec![
            bot("same", "codex", false),
            bot("same", "claude", false)
        ])
        .is_err()
    );
    let mut different = bot("second", "codex", false);
    different.selection.runtime_kind = "claude_tui".into();
    assert!(BootRoster::new(vec![bot("first", "Codex", false), different]).is_err());
}

#[test]
fn b1_dormant_census() {
    let output = std::process::Command::new("python3")
        .arg("scripts/check_legacy_supervision_census.py")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(report.contains("B1_DORMANT=PASS"));
    assert!(report.contains("activation_ready=false"));
    assert_eq!(
        report.lines().filter(|line| line.ends_with("=0")).count(),
        6
    );
}

#[test]
fn concurrent_process_install_is_one_shot() {
    let cell = Arc::new(OnceLock::new());
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let tasks: Vec<_> = (0..2)
        .map(|i| {
            let cell = cell.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                BootCohort::<usize>::install_in(
                    &cell,
                    BootRoster::new(vec![bot(&i.to_string(), "codex", false)]).unwrap(),
                )
                .is_ok()
            })
        })
        .collect();
    assert_eq!(
        tasks
            .into_iter()
            .filter_map(|task| task.join().unwrap().then_some(()))
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_provider_requests_share_worker() {
    let c = cohort(vec![
        bot("first", "codex", false),
        bot("second", "codex", false),
    ]);
    let first = Arc::new(BootSlot::begin(&c, "first").unwrap());
    let second = Arc::new(BootSlot::begin(&c, "second").unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let tasks: Vec<_> = [first.clone(), second.clone()]
        .into_iter()
        .map(|slot| {
            let calls = calls.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                slot.work_once()
                    .unwrap()
                    .run_once(move || calls.fetch_add(1, Ordering::SeqCst))
                    .await
                    .unwrap()
            })
        })
        .collect();
    let mut completed = Vec::new();
    for task in tasks {
        completed.push(task.await.unwrap());
    }
    assert!(Arc::ptr_eq(&completed[0], &completed[1]));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    first.arrive_reaped(&completed[0]).unwrap();
    second.arrive_reaped(&completed[1]).unwrap();
    c.try_start_confirmation(|_, _| Ok(()));
    c.wait_released().await.unwrap();
}
