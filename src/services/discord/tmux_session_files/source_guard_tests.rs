pub(super) fn assert_combined_marker_gate() {
    let codex =
        include_str!("../../codex.rs").to_owned() + include_str!("../../codex/absence_launch.rs");
    let combined = "crate::services::discord::stamp_spawn_markers(tmux_session_name, None)";
    let nonce_only = "crate::services::discord::write_spawn_nonce(tmux_session_name)";
    let inline_generation = "session_temp_path(tmux_session_name, \"generation\")";
    for (source, prepared) in [
        (include_str!("../../claude.rs"), 1),
        (codex.as_str(), 1),
        (include_str!("../../qwen.rs"), 0),
    ] {
        assert_eq!(source.matches(combined).count(), 1);
        assert_eq!(
            source
                .split_whitespace()
                .collect::<String>()
                .matches("stamp_spawn_markers(tmux_session_name,Some(")
                .count(),
            prepared
        );
        assert!(!source.contains(nonce_only));
        assert!(!source.contains(inline_generation));
    }
}
