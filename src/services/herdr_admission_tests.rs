use super::*;

// A seen stop file latches; resolved env opt-out stops even when the file is absent.
#[test]
fn herdr_admission_stop_file_latches_until_restart_and_any_env_value_but_on_stops() {
    let root = tempfile::tempdir().unwrap();
    let stop = root.path().join("herdr").join("admission-off");
    let admission = Admission::new(true, Some(stop.clone()));
    assert_eq!(admission.check(), Ok(()), "no env value and no stop file");
    std::fs::create_dir_all(stop.parent().unwrap()).unwrap();
    std::fs::write(&stop, b"").unwrap();
    assert_eq!(admission.check(), Err(StopCause::File));
    std::fs::remove_file(&stop).unwrap();
    assert_eq!(
        admission.check(),
        Err(StopCause::File),
        "removing the file does not reopen admission"
    );
    assert_eq!(
        Admission::new(true, None).check(),
        Err(StopCause::ProbeError),
        "no runtime root"
    );

    let open = root.path().join("absent");
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _restore =
        crate::config::TestEnvVarGuard::capture_after_shared_test_env_lock(ADMISSION_ENV);
    unsafe { std::env::remove_var(ADMISSION_ENV) };
    assert_eq!(Admission::from_env(|| Some(open.clone())).check(), Ok(()));
    for (value, expected) in [
        ("off", false),
        ("OFF", false),
        ("0", false),
        (" NO ", false),
        ("FaLsE", false),
        ("on", true),
        (" TrUe ", true),
        ("yes", true),
        ("1", true),
        ("of", true),
        ("", true),
        (" on", true),
    ] {
        unsafe { std::env::set_var(ADMISSION_ENV, value) };
        let expected = if expected {
            Ok(())
        } else {
            Err(StopCause::Env)
        };
        assert_eq!(
            Admission::from_env(|| Some(open.clone())).check(),
            expected,
            "{value:?}"
        );
    }
}

// A stop file that cannot be checked stops only that call. ENOTDIR is how unix reports a file
// standing where the herdr directory goes; Windows reports it as NotFound.
#[cfg(unix)]
#[test]
fn herdr_admission_failed_stop_file_check_stops_only_that_call() {
    let root = tempfile::tempdir().unwrap();
    let blocked = root.path().join("blocked");
    std::fs::write(&blocked, b"").unwrap();
    let unreadable = Admission::new(true, Some(blocked.join("admission-off")));
    assert_eq!(unreadable.check(), Err(StopCause::ProbeError));
    std::fs::remove_file(&blocked).unwrap();
    assert_eq!(unreadable.check(), Ok(()), "a failed check does not latch");
}
