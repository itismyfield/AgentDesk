use super::*;
use crate::services::discord::turn_finalizer::native_prepare::BootInputs;

/// Runs `work` on a single-thread runtime while a task there holds the provider custody flock and
/// releases it only once polled again, as the boot custody notice pass does across PG awaits.
fn under_held_custody_lock<T: Send + 'static>(
    root: &Path,
    work: impl std::future::Future<Output = T> + Send + 'static,
) -> T {
    let custody = root.join("discord_custody");
    fs::create_dir_all(&custody).unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(custody.join("claude.lock"))
        .unwrap();
    let (done, outcome) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(async move {
            let (held, holding) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel::<()>();
            let holder = tokio::spawn(async move {
                lock.try_lock().unwrap();
                held.send(()).unwrap();
                let _ = released.await;
                drop(lock);
            });
            holding.await.unwrap();
            let work = tokio::spawn(work);
            tokio::task::yield_now().await;
            release.send(()).unwrap();
            holder.await.unwrap();
            work.await.unwrap()
        });
        let _ = done.send(result);
    });
    outcome
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("custody wait blocked the runtime thread the lock holder needs")
}

#[test]
fn boot_preparation_waits_for_a_held_custody_lock_off_the_runtime_thread() {
    let env = Env::new();
    let root = env.dir().parent().unwrap().to_path_buf();
    let prepared = under_held_custody_lock(&root, BootInputs::prepare_off_worker(root.clone(), 2));
    assert!(prepared.is_ok());
}

#[test]
fn native_custody_record_waits_for_a_held_custody_lock_off_the_runtime_thread() {
    let env = Env::new();
    let root = env.dir().parent().unwrap().to_path_buf();
    let (f, outcome, _, _) = boot_saved(&env, 81489991, false);
    assert!(
        f.history
            .ack_native_cleanup("native-fixture", &f.ticket, &outcome)
            .unwrap()
    );
    let record = f
        .history
        .native_boot_record("native-fixture", &f.ticket)
        .unwrap();
    let written = under_held_custody_lock(
        &root,
        super::super::persist_native_boot_record_off_worker(root.clone(), record),
    );
    assert_eq!(written, Ok(()));
    fs::remove_file(root.join("injected_frames.json")).unwrap();
    assert!(
        native_boot_records(&root, &CLAUDE)
            .unwrap()
            .iter()
            .any(|restored| restored.settlement.ticket == f.ticket)
    );
}
