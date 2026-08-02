use super::*;

#[test]
fn relay_progress_heartbeat_stamps_each_confirmed_chunk() {
    let coord = TmuxRelayCoord::new(ChannelId::new(4_178));

    coord.note_relay_progress_heartbeat(1_000);
    assert_eq!(coord.last_relay_ts_ms.load(Ordering::Acquire), 1_000);

    coord.note_relay_progress_heartbeat(1_500);
    assert_eq!(coord.last_relay_ts_ms.load(Ordering::Acquire), 1_500);
    assert_eq!(coord.confirmed_end_offset.load(Ordering::Acquire), 0);
}
