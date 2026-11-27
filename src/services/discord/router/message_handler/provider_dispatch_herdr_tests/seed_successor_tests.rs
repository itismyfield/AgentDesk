//! A greeting's launch records its execution before the Pending row and the create, so a create or
//! evidence that fails still leaves that execution known; a refused record creates nothing.
use super::*;

/// One fresh launch whose successor record answers `refused`: the turn's result, and the nonce the
/// record saw with the creates run by then.
fn launch_recording(
    fx: &Fixture,
    launcher: &Arc<Launcher>,
    refused: bool,
) -> (Result<(), String>, Option<(String, usize)>) {
    let seen = Arc::new(Mutex::new(None));
    let (at, creates) = (seen.clone(), launcher.clone());
    *fx.successor.lock().unwrap() = Some(Box::new(move |nonce: &str| {
        let count = creates.creates.load(Ordering::SeqCst);
        *at.lock().unwrap() = Some((nonce.to_owned(), count));
        match refused {
            true => Err("stale seed settlement".into()),
            false => Ok(()),
        }
    }));
    let (result, _) = fx.turn(&HostedRecord::Legacy, &fx.ports(launcher), || {});
    let seen = seen.lock().unwrap().take();
    (result, seen)
}

fn recorded_before_create_survives(tag: &str, fail: impl FnOnce(&Launcher)) {
    let fx = Fixture::new(tag, None);
    let launcher = Arc::new(Launcher::default());
    fail(&launcher);
    let (result, seen) = launch_recording(&fx, &launcher, false);
    assert!(result.is_err(), "{result:?}");
    let (nonce, creates) = seen.expect("the launch recorded its successor");
    assert_eq!(creates, 0, "recorded before any create");
    let HostedRecord::Known(row) = fx.record() else {
        panic!("no stored execution");
    };
    assert_eq!(
        (row.state, row.execution_nonce),
        (HostedState::Pending, nonce)
    );
}

#[test]
fn seed_successor_recorded_before_install_survives_indeterminate_create() {
    recorded_before_create_survives("successor-lost", |launcher| {
        let lost = HerdrCreateOutcome::Indeterminate("reply EOF after the write".into());
        *launcher.create_fails.lock().unwrap() = Some(lost);
    });
}

#[test]
fn seed_successor_recorded_before_install_survives_evidence_failure() {
    recorded_before_create_survives("successor-unproven", |launcher| {
        launcher.unconfirmed.store(true, Ordering::SeqCst);
    });
}

#[test]
fn successor_record_failure_creates_nothing() {
    let fx = Fixture::new("successor-refused", None);
    let launcher = Arc::new(Launcher::default());
    let (result, seen) = launch_recording(&fx, &launcher, true);
    let error = result.unwrap_err();
    assert!(error.contains("prepare_command"), "{error}");
    assert!(seen.is_some());
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 0);
    assert_eq!(fx.record(), HostedRecord::Legacy, "no Pending row");
}
