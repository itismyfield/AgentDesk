use super::*;
use crate::config::ToolActivityDisplay;
use crate::services::discord::SharedData;
use std::sync::Arc;

pub(in crate::services::discord) const START: i64 = 1_700_000_000;

pub(in crate::services::discord) fn six_tool_fixture(
    mode: ToolActivityDisplay,
    channel: ChannelId,
) -> Arc<SharedData> {
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    let ui = &mut Arc::get_mut(&mut shared).expect("unique fixture").ui;
    ui.status_panel_v2_enabled = true;
    ui.placeholder_live_events = Arc::new(PlaceholderLiveEvents::with_tool_activity_display(mode));
    for i in 0..6 {
        let id = format!("footer-tool-{i}");
        ui.placeholder_live_events.push_status_events(
            channel,
            status_events_from_tool_use_with_id(
                "Bash",
                &serde_json::json!({"command":format!("cargo test -p fixture{i} -- --nocapture")})
                    .to_string(),
                Some(&id),
            ),
        );
        ui.placeholder_live_events.push_status_events(
            channel,
            status_events_from_tool_result_with_id(Some("Bash"), false, Some(&id)),
        );
    }
    ui.placeholder_live_events
        .last_recent_event_unix
        .insert(channel, START + 2);
    shared
}

pub(in crate::services::discord) fn rollover_splits(
    body: &str,
    mut next: impl FnMut(&str, usize) -> Option<usize>,
) -> Vec<usize> {
    let mut offset = 0;
    let mut splits = Vec::new();
    while let Some(split) = next(&body[offset..], offset) {
        assert!(split > 0 && split < body.len() - offset);
        offset += split;
        splits.push(offset);
    }
    splits
}
