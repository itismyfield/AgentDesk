use super::*;

// A seen stop file keeps admission stopped after it is removed; no runtime root stops the call;
// any env value but `on` stops whatever the file says.
#[test]
fn herdr_admission_stop_file_latches_until_restart_and_any_env_value_but_on_stops() {
    let root = tempfile::tempdir().unwrap();
    let stop = root.path().join("herdr").join("admission-off");
    let admission = Admission::new(None, Some(stop.clone()));
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
        Admission::new(None, None).check(),
        Err(StopCause::ProbeError),
        "no runtime root"
    );

    let open = root.path().join("absent");
    let env = |value: &str| Admission::new(Some(value.as_ref()), Some(open.clone())).check();
    assert_eq!(env("on"), Ok(()));
    for value in ["off", "OFF", "of", "", " on"] {
        assert_eq!(env(value), Err(StopCause::Env), "{value:?}");
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
    let unreadable = Admission::new(None, Some(blocked.join("admission-off")));
    assert_eq!(unreadable.check(), Err(StopCause::ProbeError));
    std::fs::remove_file(&blocked).unwrap();
    assert_eq!(unreadable.check(), Ok(()), "a failed check does not latch");
}
