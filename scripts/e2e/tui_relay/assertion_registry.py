"""Supported assertion operations and their accepted option names."""

ASSERTION_OPTIONS = {
    **{k: None for k in ("text_present", "provider_text_present", "raw_text_present", "no_duplicate_marker", "ordered_text_present",
                         "no_duplicate_content", "no_resume_prompt_chrome", "no_suppressed_label_chrome",
                         "no_control_chars", "body_not_overwritten", "fixture_followup_ready",
                         "fixture_no_health_degradation", "no_placeholder_left", "no_status_chrome")},
    "message_count_between_markers": {"min", "max"}, "raw_message_count_between_markers": {"min", "max", "include_our_send"},
    "relay_latency_within": {"max_seconds"}, "body_complete": {"head", "tail"},
    "chrome_count": {"text", "regex", "min", "max", "exact", "include_our_send"}, "raw_text_absent": {"needle", "text", "include_our_send"},
    "marker_absent": {"marker", "surface", "include_our_send"}, "provider_hold_marker_seen": {"marker", "ok_marker"},
    "status_panel_after_body": {"body_marker", "panel_regex"}, "single_status_panel": {"panel_regex"},
    "completion_chrome_after_body": {"body_marker", "required"}, "relay_bodies_after_local_control": {"containing", "exact"},
    "fixture_task_notification": {"source", "kind", "status"}, "fixture_finalized": {"finalized", "result_text_source", "active_turn"},
    "fixture_task_complete_finalized": {"result_text_source", "turn_id"}, "fixture_state": {"followup_probe_accepted"},
    "no_duplicate_marker_with_known_gap": {"marker", "known_gap"},
    "deliver_result": {"delivery", "inject_veto"}, "completion_per_turn": {"exact", "marker"},
    "turn_summary_after_body": {"body_marker", "header", "before_marker", "tool_label", "output_owner"}, "autonomous_background_turn": {"armed_marker", "auto_marker"},
    "utf16_chunk_integrity": {"max_units", "min_chunks"},
}
