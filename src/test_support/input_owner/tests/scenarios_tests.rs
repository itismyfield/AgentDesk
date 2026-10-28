use serde_json::Value;

pub(crate) fn writable_r5_counts(value: &Value) -> Result<(), &'static str> {
    for name in [
        "process_boots",
        "r5_admissions",
        "r5_queue_writes",
        "r5_content",
        "forward_recovery",
    ] {
        if value["evidence"][name]
            .as_u64()
            .is_none_or(|count| count == 0)
        {
            return Err(
                "writable R5 requires measured positive boot/admission/write/input/recovery",
            );
        }
    }
    if value["evidence"]["r5_content"] != 1 {
        return Err("one ready R5 input must produce exactly one content effect");
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct InputExpectation {
    pub source: u64,
    pub generation: u64,
    pub nonce: String,
    pub execution: String,
    pub admissions: u64,
    pub content: u64,
    pub body: String,
}

pub(crate) fn input_expectations() -> Vec<InputExpectation> {
    let spec: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/input_owner_rollback/fixtures/scenarios.json"
    )))
    .unwrap();
    let inputs = &spec["inputs"];
    (0..inputs["count"].as_u64().unwrap())
        .map(|i| InputExpectation {
            source: inputs["first_source"].as_u64().unwrap() + i,
            generation: inputs["queued_generation"].as_u64().unwrap(),
            nonce: format!("{}{i}", inputs["attempt_nonce_prefix"].as_str().unwrap()),
            execution: inputs["execution_nonce"].as_str().unwrap().into(),
            admissions: inputs["expected_admissions_per_source"].as_u64().unwrap(),
            content: inputs["expected_content_per_source"].as_u64().unwrap(),
            body: inputs["body"].as_str().unwrap().into(),
        })
        .collect()
}

#[test]
fn s5_fixture_writable_r5_cannot_pass_with_absent_or_zero_effects() {
    let mut value: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/input_owner_rollback/manifest.template.json"
    )))
    .unwrap();
    assert!(writable_r5_counts(&value).is_err());
    for key in [
        "process_boots",
        "r5_admissions",
        "r5_queue_writes",
        "r5_content",
        "forward_recovery",
    ] {
        value["evidence"][key] = 1.into();
    }
    assert!(
        writable_r5_counts(&value).is_ok(),
        "counter checker self-test only; no R5 binary exists yet"
    );
    value["evidence"]["r5_content"] = 2.into();
    assert!(
        writable_r5_counts(&value).is_err(),
        "duplicate R5 content cannot pass"
    );
    value["evidence"]["r5_content"] = 1.into();

    for key in [
        "process_boots",
        "r5_admissions",
        "r5_queue_writes",
        "r5_content",
        "forward_recovery",
    ] {
        value["evidence"][key] = 0.into();
        assert!(
            writable_r5_counts(&value).is_err(),
            "zero {key} cannot pass"
        );
        value["evidence"][key] = Value::Null;
        assert!(
            writable_r5_counts(&value).is_err(),
            "missing {key} cannot pass"
        );
        value["evidence"][key] = 1.into();
    }
}

#[test]
fn s5_fixture_5001_source_nonce_expectations_and_crash_cuts_are_complete() {
    let spec: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/input_owner_rollback/fixtures/scenarios.json"
    )))
    .unwrap();
    let expected = input_expectations();
    assert_eq!(expected.len(), 5001);
    assert_eq!(
        expected
            .iter()
            .map(|row| row.source)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        5001
    );
    assert_eq!(
        expected
            .iter()
            .map(|row| &row.nonce)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        5001
    );
    assert!(expected.iter().all(|row| row.generation == 5
        && row.admissions == 1
        && row.content == 1
        && row.execution == "execution-A"
        && row.body == "same literal _ 한글"));
    let cuts = spec["crash_cuts"].as_array().unwrap();
    assert_eq!(cuts.len(), 7);
    for cut in cuts {
        assert_eq!(cut["same_ticket"], true);
        assert_eq!(
            cut["max_recovery_content"].as_u64().unwrap(),
            u64::from(!cut["possibly_sent"].as_bool().unwrap())
        );
    }
    assert_eq!(spec["readback_cases"].as_array().unwrap().len(), 4);
}
