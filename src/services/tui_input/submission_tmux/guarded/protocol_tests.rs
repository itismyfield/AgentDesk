use super::*;

fn classify_enter(result: Result<Output, String>) -> EffectOutcome {
    classify_effect(result, APPLIED, HELD)
}

fn output(stdout: &[u8]) -> Output {
    Output {
        status: Default::default(),
        stdout: stdout.into(),
        stderr: Vec::new(),
    }
}

#[test]
fn cg_b2a_snapshot_rejects_unknown_geometry_and_identity() {
    let good = "$1 @2 %3 160 40 manual 1 0 1234 5678 0 0 0 0\n";
    assert!(Geometry::read(output(good.as_bytes())).is_some());
    for raw in [
        "$1 @2 %3 159 40 manual 1 0 1234 5678 0 0 0 0",
        "$1 @2 %3 160 39 manual 1 0 1234 5678 0 0 0 0",
        "$1 @2 %3 160 40 latest 1 0 1234 5678 0 0 0 0",
        "$1 @2 %3 160 40 manual 2 0 1234 5678 0 0 0 0",
        "$1 @2 %3 160 40 manual 1 1 1234 5678 0 0 0 0",
        "$1 @2 %3 160 40 manual 1 0 1234 0 0 0 0 0",
        "$1 @2 %3 160 40 manual 1 0 0 5678 0 0 0 0",
        "$1 @2 %3 160 40 manual 1 0 1234",
        "$1 @2 %3 160 40 manual 1 0 1234 5678 0 0 0 0 extra",
        "$1 @2 %3;send-keys 160 40 manual 1 0 1234 5678 0 0 0 0",
        "$1 @2 % 160 40 manual 1 0 1234 5678 0 0 0 0",
        "$1 @2 %3 NaN 40 manual 1 0 1234 5678 0 0 0 0",
        "$1 @2 %3 160 40 manual 1 0 1234 5678 1 0 0 0",
        "$1 @2 %3 160 40 manual 1 0 1234 5678 0 1 0 0",
        "$1 @2 %3 160 40 manual 1 0 1234 5678 0 0 1 0",
        "$1 @2 %3 160 40 manual 1 0 1234 5678 0 0 0 1",
    ] {
        assert!(Geometry::read(output(raw.as_bytes())).is_none(), "{raw}");
    }
    assert!(Geometry::read(output(b"\xff")).is_none());
    let mut warning = output(good.as_bytes());
    warning.stderr = b"warning".into();
    assert!(Geometry::read(warning).is_none());
}

#[test]
fn cg_b2b_atomic_capture_requires_matching_first_snapshot_and_preserves_body() {
    let good = "$1 @2 %3 160 40 manual 1 0 1234 5678 0 0 0 0";
    let geometry = Geometry::read(output(good.as_bytes())).unwrap();
    let body = "a  b\nexplicit row\n\n";
    assert_eq!(
        read_capture(output(format!("{good}\n{body}").as_bytes()), &geometry),
        Some(body.into())
    );
    for first in [
        good.replace("160 40", "161 40"),
        good.replace("%3", "%4"),
        good.replace("1234", "9999"),
        good.replace("0 0 0 0", "1 0 0 0"),
        "unavailable".to_string(),
    ] {
        assert!(read_capture(output(format!("{first}\n{body}").as_bytes()), &geometry).is_none());
    }
    assert!(read_capture(output(good.as_bytes()), &geometry).is_none());
    let mut warning = output(format!("{good}\n{body}").as_bytes());
    warning.stderr = b"warning".into();
    assert!(read_capture(warning, &geometry).is_none());
}

#[test]
fn cg_b2b_payload_acknowledgements_are_separate_from_enter() {
    assert_eq!(
        classify_effect(Ok(output(PAYLOAD_APPLIED)), PAYLOAD_APPLIED, PAYLOAD_HELD),
        EffectOutcome::Applied
    );
    assert_eq!(
        classify_effect(Ok(output(PAYLOAD_HELD)), PAYLOAD_APPLIED, PAYLOAD_HELD),
        EffectOutcome::Held
    );
    for raw in [
        APPLIED,
        HELD,
        b"CG_PAYLOAD_APPLIED",
        b"CG_PAYLOAD_APPLIED\nextra",
        b"",
    ] {
        assert!(matches!(
            classify_effect(Ok(output(raw)), PAYLOAD_APPLIED, PAYLOAD_HELD),
            EffectOutcome::Unknown(_)
        ));
    }
}

#[test]
fn cg_b2a_only_exact_branch_acknowledgements_are_definite() {
    assert_eq!(classify_enter(Ok(output(APPLIED))), EffectOutcome::Applied);
    assert_eq!(classify_enter(Ok(output(HELD))), EffectOutcome::Held);
    for raw in [
        b"".as_slice(),
        b"CG_ENTER_APPLIED",
        b"CG_ENTER_APPLIED\nextra",
        b"CG_ENTER_HELD\nCG_ENTER_APPLIED\n",
        b"\xff",
    ] {
        assert!(
            matches!(classify_enter(Ok(output(raw))), EffectOutcome::Unknown(_)),
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
        EffectOutcome::Unknown(_)
    ));
    let mut warning = output(APPLIED);
    warning.stderr = b"warning".into();
    assert!(matches!(
        classify_enter(Ok(warning)),
        EffectOutcome::Unknown(_)
    ));
    for error in [
        "spawn failed",
        "timeout after possible send",
        "I/O acknowledgement loss",
    ] {
        assert_eq!(
            classify_enter(Err(error.into())),
            EffectOutcome::Unknown(error.into())
        );
    }
}
