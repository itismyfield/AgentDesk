use super::{remove_key, top_bool, upsert};
use sha2::Digest;

const UPSTREAM_COMMIT: &str = "7b116c05bfda646af39d2524c54e70c751f57ee8";
const UPSTREAM_IO_SHA256: &str = "a4b7447c874f9f7d10d93485dd6f1e8fd8d44d1933a0492f1e5208d6d06b099b";
const UPSTREAM_WRITE_SHA256: &str =
    "d72f1d49947b03406b0c19da4a56eb73d16b42b08309b78386dc1dd47c7a2c73";
const FUNCTIONS_SHA256: &str = "34d68ba09d624a32d0dee3b350dfa9696081e19b5b065c12f84f70ff7d2c38fe";
const UNIT_TESTS_SHA256: &str = "9e6c2c36b6885e2b54f6e91a16d60d98e86359ded13ac0a7e8cab5f9856d44b2";
const UNIT_OUTPUTS_SHA256: &str =
    "91e599c5c234c990365f2c6698d58db0b43ccbd581d0bb27a30b5ca4900ef382";
const FUNCTIONS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/herdr_writer/upstream_functions.rs"
));
const UNIT_TESTS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/herdr_writer/upstream_unit_tests.txt"
));
const UNIT_OUTPUTS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/herdr_writer/upstream_unit_outputs.json"
));
const PROVENANCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/herdr_writer/provenance.json"
));

mod pinned {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/herdr_writer/upstream_functions.rs"
    ));

    pub(super) fn section(content: &str, section: &str, key: &str, value: &str) -> String {
        upsert_section_raw(content, section, key, value)
    }
}

#[test]
fn pinned_upstream_writer_fixtures_have_recorded_sha256() {
    let provenance: serde_json::Value = serde_json::from_str(PROVENANCE).unwrap();
    assert_eq!(provenance["commit"], UPSTREAM_COMMIT);
    assert_eq!(
        provenance["sources"]["src/config/io.rs"]["sha256"],
        UPSTREAM_IO_SHA256
    );
    assert_eq!(
        provenance["sources"]["src/config/write.rs"]["sha256"],
        UPSTREAM_WRITE_SHA256
    );
    for (name, source, expected) in [
        ("upstream_functions.rs", FUNCTIONS, FUNCTIONS_SHA256),
        ("upstream_unit_tests.txt", UNIT_TESTS, UNIT_TESTS_SHA256),
        (
            "upstream_unit_outputs.json",
            UNIT_OUTPUTS,
            UNIT_OUTPUTS_SHA256,
        ),
    ] {
        assert_eq!(
            format!("{:x}", sha2::Sha256::digest(source.as_bytes())),
            expected,
            "pinned_upstream_fixture_sha256_contract: {name}"
        );
        assert_eq!(provenance["fixtures"][name]["sha256"], expected);
    }
}

#[test]
fn pinned_upstream_unit_outputs_match_the_replica() {
    let cases: serde_json::Value = serde_json::from_str(UNIT_OUTPUTS).unwrap();
    for case in cases.as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let key = case["key"].as_str().unwrap();
        assert!(
            UNIT_TESTS.contains(&serde_json::to_string(input).unwrap())
                && UNIT_TESTS.contains(case["upstream_test"].as_str().unwrap()),
            "pinned_upstream_unit_input_contract"
        );
        let (oracle, actual) = match case["operation"].as_str().unwrap() {
            "top_bool" => {
                let value = case["value"].as_bool().unwrap();
                (
                    pinned::upsert_top_level_bool(input, key, value),
                    top_bool(input, key, value),
                )
            }
            "upsert_bool" => {
                let section = case["section"].as_str().unwrap();
                let value = case["value"].as_bool().unwrap().to_string();
                (
                    pinned::section(input, section, key, &value),
                    upsert(input, section, key, &value),
                )
            }
            "remove_key" => {
                let section = case["section"].as_str().unwrap();
                (
                    pinned::remove_section_key(input, section, key),
                    remove_key(input, section, key),
                )
            }
            operation => panic!("unknown pinned upstream operation: {operation}"),
        };
        let expected = case["expected"].as_str().unwrap();
        assert_eq!(
            oracle, expected,
            "pinned_upstream_unit_output_fixture_contract: {}",
            case["upstream_test"]
        );
        assert_eq!(
            actual, expected,
            "pinned_upstream_unit_output_contract: {}",
            case["upstream_test"]
        );
    }
}

#[test]
fn pinned_writer_golden_cases_cover_insert_replace_and_noncanonical_headers() {
    for raw in [
        "",
        "[session]\nresume_agents_on_restore=true # old\n",
        "[theme]\nname='dark'\n",
        "[session] # comment\nresume_agents_on_restore=true\n",
    ] {
        assert_eq!(
            upsert(raw, "session", "resume_agents_on_restore", "false"),
            pinned::section(raw, "session", "resume_agents_on_restore", "false"),
            "pinned_writer_exactness_contract"
        );
    }
}

#[test]
fn pinned_upstream_section_boundary_requires_a_closing_bracket() {
    let content = concat!(
        "[session]\n",
        "banner = '''\n",
        "[inside a multiline string\n",
        "'''\n",
        "resume_agents_on_restore = true\n",
        "[theme]\n",
        "name = \"default\"\n",
    );
    assert_eq!(
        upsert(content, "session", "resume_agents_on_restore", "false"),
        pinned::section(content, "session", "resume_agents_on_restore", "false"),
        "pinned_upstream_section_boundary_contract"
    );
}

#[test]
fn pinned_upstream_leading_bom_is_removed_before_each_writer_edit() {
    let session =
        "\u{feff}[session]\nresume_agents_on_restore = false\nname = 'keep\u{feff}data'\n";
    let toast = "\u{feff}[ui.toast]\nenabled = true\ndelivery = 'off'\n";
    let onboarding = "\u{feff}onboarding = true\n[terminal]\ndefault_shell = 'pwsh.exe'\n";
    for (actual, expected) in [
        (
            upsert(session, "session", "resume_agents_on_restore", "false"),
            pinned::section(
                &session['\u{feff}'.len_utf8()..],
                "session",
                "resume_agents_on_restore",
                "false",
            ),
        ),
        (
            remove_key(toast, "ui.toast", "enabled"),
            pinned::remove_section_key(&toast['\u{feff}'.len_utf8()..], "ui.toast", "enabled"),
        ),
        (
            top_bool(onboarding, "onboarding", false),
            pinned::upsert_top_level_bool(
                &onboarding['\u{feff}'.len_utf8()..],
                "onboarding",
                false,
            ),
        ),
    ] {
        assert_eq!(actual, expected, "pinned_upstream_leading_bom_contract");
    }
    assert!(
        super::verify(session).is_ok(),
        "pinned_upstream_leading_bom_replay_contract"
    );
}
