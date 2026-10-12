use super::*;

fn output(stdout: &[u8]) -> Output {
    Output {
        status: Default::default(),
        stdout: stdout.into(),
        stderr: Vec::new(),
    }
}

#[test]
fn cg_b2a_snapshot_rejects_unknown_geometry_and_identity() {
    let good = "$1 @2 %3 160 40 manual 1 0 1234 5678\n";
    assert!(Geometry::read(output(good.as_bytes())).is_some());
    for raw in [
        "$1 @2 %3 159 40 manual 1 0 1234 5678",
        "$1 @2 %3 160 39 manual 1 0 1234 5678",
        "$1 @2 %3 160 40 latest 1 0 1234 5678",
        "$1 @2 %3 160 40 manual 2 0 1234 5678",
        "$1 @2 %3 160 40 manual 1 1 1234 5678",
        "$1 @2 %3 160 40 manual 1 0 1234 0",
        "$1 @2 %3 160 40 manual 1 0 0 5678",
        "$1 @2 %3 160 40 manual 1 0 1234",
        "$1 @2 %3 160 40 manual 1 0 1234 5678 extra",
        "$1 @2 %3;send-keys 160 40 manual 1 0 1234 5678",
        "$1 @2 % 160 40 manual 1 0 1234 5678",
        "$1 @2 %3 NaN 40 manual 1 0 1234 5678",
    ] {
        assert!(Geometry::read(output(raw.as_bytes())).is_none(), "{raw}");
    }
    assert!(Geometry::read(output(b"\xff")).is_none());
    let mut warning = output(good.as_bytes());
    warning.stderr = b"warning".into();
    assert!(Geometry::read(warning).is_none());
}

#[test]
fn cg_b2a_only_exact_branch_acknowledgements_are_definite() {
    assert_eq!(classify_enter(Ok(output(APPLIED))), InputRun::Applied);
    assert_eq!(classify_enter(Ok(output(HELD))), InputRun::Refused(held()));
    for raw in [
        b"".as_slice(),
        b"CG_ENTER_APPLIED",
        b"CG_ENTER_APPLIED\nextra",
        b"CG_ENTER_HELD\nCG_ENTER_APPLIED\n",
        b"\xff",
    ] {
        assert!(
            matches!(
                classify_enter(Ok(output(raw))),
                InputRun::Indeterminate { confirmed: 0, .. }
            ),
            "{raw:?}"
        );
    }
    let mut failed = output(APPLIED);
    failed.status = std::process::Command::new("sh")
        .args(["-c", "exit 1"])
        .status()
        .unwrap();
    assert!(matches!(
        classify_enter(Ok(failed)),
        InputRun::Indeterminate { confirmed: 0, .. }
    ));
    let mut warning = output(APPLIED);
    warning.stderr = b"warning".into();
    assert!(matches!(
        classify_enter(Ok(warning)),
        InputRun::Indeterminate { confirmed: 0, .. }
    ));
    for error in [
        "spawn failed",
        "timeout after possible send",
        "I/O acknowledgement loss",
    ] {
        assert_eq!(
            classify_enter(Err(error.into())),
            InputRun::Indeterminate {
                confirmed: 0,
                cause: StopCause::Send(error.into())
            }
        );
    }
}
