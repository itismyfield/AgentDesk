use super::upsert;

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
            upsert(raw, "session", "resume_agents_on_restore", "false"),
            expected,
            "pinned_writer_exactness_contract"
        );
    }
}
