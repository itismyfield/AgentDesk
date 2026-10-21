use super::*;

#[test]
fn editor_changes_only_the_boolean_token_and_keeps_utf8_crlf_comments() {
    for raw in [
        "# 머리말\r\n[session]\r\nresume_agents_on_restore = true # 주석\r\n[theme]\r\nname='dark'\r\n",
        "[session]\nresume_agents_on_restore=true\n",
        "[session]\nresume_agents_on_restore =\ttrue\t# true in comment\n",
    ] {
        let token = raw.find("true").unwrap();
        let mut expected = raw.to_owned();
        expected.replace_range(token..token + 4, "false");
        assert_eq!(candidate(raw), Ok(expected), "boolean_token_only_contract");
    }
    let raw = "[session]\r\nresume_agents_on_restore = false # retain\r\n";
    assert_eq!(
        candidate(raw),
        Ok(raw.into()),
        "already_false_bytes_contract"
    );
}

#[test]
fn editor_inserts_only_into_a_semantically_identified_bare_section() {
    for (raw, expected) in [
        ("", "[session]\nresume_agents_on_restore = false\n"),
        (
            "# retained",
            "# retained\n[session]\nresume_agents_on_restore = false\n",
        ),
        (
            "[theme]\r\nname='dark'\r\n",
            "[theme]\r\nname='dark'\r\n[session]\r\nresume_agents_on_restore = false\r\n",
        ),
        (
            "[session]\r\n[theme]\r\nname='dark'\r\n",
            "[session]\r\nresume_agents_on_restore = false\r\n[theme]\r\nname='dark'\r\n",
        ),
        ("[session]", "[session]\nresume_agents_on_restore = false\n"),
    ] {
        assert_eq!(
            candidate(raw),
            Ok(expected.into()),
            "canonical_insertion_contract"
        );
    }
    assert_eq!(
        candidate("[session]\n[session.child]\nx=1\n"),
        Ok("[session]\nresume_agents_on_restore = false\n[session.child]\nx=1\n".into()),
        "key_insertion_precedes_child_table_contract"
    );
    let raw = "text = '''\n[session]\n'''\n[session]\n";
    let expected = "text = '''\n[session]\n'''\n[session]\nresume_agents_on_restore = false\n";
    assert_eq!(
        candidate(raw),
        Ok(expected.into()),
        "header_inside_string_is_not_an_insertion_site_contract"
    );
}

#[test]
fn editor_refuses_noncanonical_writer_shapes_even_when_already_false() {
    for raw in [
        "session.resume_agents_on_restore = true\n",
        "session = { resume_agents_on_restore = true }\n",
        "[session] # comment\nresume_agents_on_restore = true\n",
        "['session']\nresume_agents_on_restore = false\n",
        "[session]\n'resume_agents_on_restore' = true\n",
        "[session]\n\"resume_agents_on_restore\" = false\n",
        "session = {}\n",
        "[session] # comment\n",
        "[session.child]\na=1\n",
    ] {
        assert_eq!(
            candidate(raw),
            Err(EditError::WriterIncompatible),
            "noncanonical_writer_refusal_contract: {raw}"
        );
    }
}

#[test]
fn editor_refuses_invalid_duplicate_and_non_boolean_policy() {
    for raw in [
        "[session]\nresume_agents_on_restore=true\nresume_agents_on_restore=false\n",
        "[session",
        "[session]\nresume_agents_on_restore='true'\n",
        "[session]\nresume_agents_on_restore=1\n",
        "session = false\n",
    ] {
        assert!(
            candidate(raw).is_err(),
            "invalid_policy_refusal_contract: {raw}"
        );
    }
}

#[test]
fn writer_replay_preserves_false_across_theme_sound_toast_and_onboarding() {
    let mut current = "[session]\r\nresume_agents_on_restore=false\r\n".to_owned();
    for (section, key, value) in [
        ("theme", "name", "\"dark\""),
        ("theme", "auto_switch", "false"),
        ("ui.sound", "enabled", "true"),
        ("ui.toast", "delivery", "\"herdr\""),
        ("session", "resume_agents_on_restore", "false"),
    ] {
        current = writer_compat::upsert(&current, section, key, value);
        assert!(
            is_false(&current),
            "current_text_writer_preserves_false_contract"
        );
    }
    let onboarding = writer_compat::top_bool(&current, "onboarding", false);
    assert!(
        is_false(&onboarding),
        "onboarding_reads_current_text_contract"
    );
    assert_eq!(writer_compat::verify(&current), Ok(()));
}

#[test]
fn writer_replay_fixture_matches_pinned_section_boundary_behavior() {
    let raw = "[session]\n# retained\n[theme]\nname='dark'\n";
    assert_eq!(
        writer_compat::upsert(raw, "session", "resume_agents_on_restore", "false"),
        "[session]\n# retained\nresume_agents_on_restore = false\n[theme]\nname='dark'\n"
    );
    let raw = "session={resume_agents_on_restore=false}\n";
    let replay = writer_compat::upsert(raw, "session", "resume_agents_on_restore", "false");
    assert!(!is_false(&replay), "writer_compatibility_guard_contract");
}

#[test]
fn semantic_false_recognizes_inline_dotted_and_quoted_paths_without_editing_them() {
    for raw in [
        "session={resume_agents_on_restore=false}",
        "session.resume_agents_on_restore=false",
        "['session']\n'resume_agents_on_restore'=false",
    ] {
        assert!(is_false(raw), "semantic_false_inline_item_contract: {raw}");
        assert_eq!(
            candidate(raw),
            Err(EditError::WriterIncompatible),
            "writer_replay_is_required_even_for_false_contract"
        );
    }
}

#[test]
fn pinned_writer_golden_cases_cover_insert_replace_and_noncanonical_headers() {
    for (raw, expected) in [
        ("", "[session]\nresume_agents_on_restore = false\n"),
        (
            "[session]\nresume_agents_on_restore=true # old\n",
            "[session]\nresume_agents_on_restore = false\n",
        ),
        (
            "[theme]\nname='dark'\n",
            "[theme]\nname='dark'\n\n[session]\nresume_agents_on_restore = false\n",
        ),
        (
            "[session] # comment\nresume_agents_on_restore=true\n",
            "[session] # comment\nresume_agents_on_restore=true\n\n[session]\nresume_agents_on_restore = false\n",
        ),
    ] {
        assert_eq!(
            writer_compat::upsert(raw, "session", "resume_agents_on_restore", "false"),
            expected,
            "pinned_writer_exactness_contract"
        );
    }
}

#[test]
fn personal_apply_modules_are_absent_from_production_compilation() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/services/session_host/herdr/policy/mod.rs"),
    )
    .unwrap();
    for module in ["editor", "writer_compat", "apply"] {
        let pattern = format!(
            r"#\[cfg\(all\(test,\s*unix\)\)\]\s*(?:pub(?:\(crate\))?\s+)?mod\s+{module}\s*;"
        );
        assert!(
            regex::Regex::new(&pattern).unwrap().is_match(&source),
            "personal_config_apply_production_zero_contract: {module}"
        );
    }
}

#[test]
fn pinned_toast_removal_replay_keeps_the_current_resume_policy() {
    let current = "[session]\nresume_agents_on_restore=false\n[ui.toast]\nenabled=true\n";
    let updated = writer_compat::upsert(current, "ui.toast", "delivery", "\"off\"");
    let result = writer_compat::remove_key(&updated, "ui.toast", "enabled");
    assert!(!result.contains("enabled=true"));
    assert!(
        is_false(&result),
        "toast_remove_enabled_reads_current_text_contract"
    );
}
