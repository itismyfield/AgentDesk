mod tests {
    use super::super::DeliveryLeaseKey;
    use poise::serenity_prelude::ChannelId;

    fn fallback_key(channel: ChannelId, offset: u64, site: &'static str) -> DeliveryLeaseKey {
        DeliveryLeaseKey::new_for_site_with_fallback_offset(
            channel,
            33,
            0,
            None,
            None,
            Some(offset),
            site,
        )
    }

    #[test]
    fn id_zero_without_inflight_uses_observed_range_start_4277() {
        let channel = ChannelId::new(4_277);
        let first = fallback_key(channel, 2_484_989, "watcher");
        let same = fallback_key(channel, 2_484_989, "idle_tail");
        let next = fallback_key(channel, 2_486_000, "watcher");

        assert!(!first.is_degenerate_legacy());
        assert_eq!(
            first, same,
            "both relay owners must contend on one turn key"
        );
        assert_ne!(first, next, "a later TUI-direct turn needs a distinct key");
    }

    #[test]
    fn watcher_and_idle_tail_cannot_acquire_same_fallback_turn_concurrently_4277() {
        let channel = ChannelId::new(4_278);
        let cell = super::super::DeliveryLeaseCell::new(channel);
        let watcher_key = fallback_key(channel, 900, "watcher");
        let idle_key = fallback_key(channel, 900, "idle_tail");

        assert!(cell.try_acquire(
            watcher_key,
            super::super::LeaseHolder::Watcher { instance_id: 1 },
            900,
            1_200,
            super::super::lease_now_ms().saturating_add(1_000),
        ));
        assert!(
            !cell.try_acquire(
                idle_key,
                super::super::LeaseHolder::Sink,
                900,
                1_200,
                super::super::lease_now_ms().saturating_add(1_000),
            ),
            "the second relay owner must lose the one-turn delivery lease"
        );
    }
}
