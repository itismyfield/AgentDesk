//! Legacy's Codex delivery records as late adoption reads them: only the current tmux generation's.

use super::*;
use crate::services::discord::outbound::delivery_record::{
    ConfirmedDeliveryReceipt, DeliveredCommit, ExactJsonlSourceIdentity,
};

/// Stamps `tmux`'s generation marker at `unix_secs` and returns the generation Legacy reads.
fn generation(tmux: &str, unix_secs: i64) -> i64 {
    let path = crate::services::tmux_common::session_temp_path(tmux, "generation");
    let parent = std::path::Path::new(&path).parent().unwrap().to_owned();
    std::fs::create_dir_all(parent).unwrap();
    std::fs::write(&path, "generation").unwrap();
    filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(unix_secs, 7)).unwrap();
    dr::current_generation_mtime_ns(tmux)
}

/// The Discord message id created at `unix_secs`.
fn message_at(unix_secs: u64) -> u64 {
    (unix_secs * 1000 - 1_420_070_400_000) << 22
}

fn delivery(
    (channel, tmux): (u64, &str),
    range: (u64, u64),
    generation: i64,
    message_id: u64,
) -> (DeliveredCommit, ConfirmedDeliveryReceipt) {
    let frontier = DeliveredCommit {
        range,
        generation_mtime_ns: generation,
        attempts: 1,
        panel_msg_id: Some(message_id),
        panel_channel_id: Some(channel),
    };
    let source = ExactJsonlSourceIdentity {
        provider: ProviderKind::Codex.as_str().into(),
        tmux_session_name: tmux.into(),
        turn_nonce: "turn".into(),
        range,
        generation_mtime_ns: generation,
        offset_authority_channel_id: channel,
        delivery_channel_id: channel,
    };
    let receipt = ConfirmedDeliveryReceipt {
        source,
        delivery_channel_id: channel,
        message_id,
    };
    (frontier, receipt)
}

/// A frontier and a receipt of the tmux's first generation give nothing once a second generation
/// of the same tmux name began; that generation's own delivery is given, dated by its message.
#[test]
fn only_the_current_generation_of_the_same_tmux_gives_codex_records() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let (channel, tmux) = (7_651_401, "AgentDesk-codex-records");
    let g1 = generation(tmux, 1_700_000_000);
    let (frontier, receipt) = delivery((channel, tmux), (10, 20), g1, message_at(1_700_000_100));
    dr::write_confirmed_delivery(&ProviderKind::Codex, channel, frontier, receipt).unwrap();
    let in_g1 = codex_records(channel, tmux);
    assert_eq!(in_g1.receipts.len(), 1);
    assert_eq!(codex_records(channel, "AgentDesk-codex-other").receipts, []);

    let g2 = generation(tmux, 1_800_000_000);
    let in_g2 = codex_records(channel, tmux);
    let since = chrono::DateTime::from_timestamp_nanos(g2);
    assert_eq!(in_g2.generation_since, Some(since));
    assert_eq!(in_g2.frontier, None, "a G1 frontier covers no G2 turn");
    assert_eq!(in_g2.receipts, [], "a G1 receipt covers no G2 turn");

    let id = message_at(1_800_000_100);
    let (frontier, receipt) = delivery((channel, tmux), (0, 7), g2, id);
    dr::write_confirmed_delivery(&ProviderKind::Codex, channel, frontier, receipt).unwrap();
    let delivered = CodexDelivery {
        range: (0, 7),
        at: chrono::DateTime::from_timestamp(1_800_000_100, 0),
    };
    let in_g2 = codex_records(channel, tmux);
    assert_eq!(in_g2.frontier, Some(delivered.clone()));
    assert_eq!(in_g2.receipts, [delivered]);
}
