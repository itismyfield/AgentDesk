use std::fs;

use serenity::model::id::ChannelId;

use crate::services::discord::busy_followup_retry_store::sweep_test_support::{
    legacy, load_in_root, save_in_root, sweep_expired_in_root_at, with_root,
};
use crate::services::discord::busy_followup_retry_store::{BusyFollowupRetryState, release_notice};
use crate::services::discord::{InflightTurnState, runtime_store};
use crate::services::provider::ProviderKind;

/// Checks that age removes only the unspent binding after a terminal answer.
#[test]
fn sweeps_delete_only_unspent_bindings_however_old() {
    with_root(|| {
        let provider = ProviderKind::Claude;
        let root = runtime_store::discord_busy_followup_retries_root().expect("store root");
        let now = 1_000 + 25 * 60 * 60 * 1_000;
        let channel = root.join("claude/48884");
        let seed = |id: u64, state: &BusyFollowupRetryState| {
            save_in_root(&root, &provider, 48_884, id, state).expect("seed");
        };
        seed(48_885, &legacy(900, 1, 1_000));
        seed(48_886, &legacy(901, 0, 1_000));
        let capped = BusyFollowupRetryState::charge(None, 902, 300_000, 1_000, None);
        seed(48_887, &capped);
        fs::write(channel.join("48888.json"), b"{").expect("unreadable record");
        let mut answered = InflightTurnState::new(
            provider.clone(),
            48_884,
            None,
            7,
            48_887,
            902,
            "answered".into(),
            None,
            None,
            None,
            None,
            0,
        );
        answered.busy_followup_retry_user_msg_id = 48_887;
        release_notice(&provider, ChannelId::new(48_884), &answered);
        let capped = BusyFollowupRetryState {
            notice_bound: false,
            ..capped
        };
        assert_eq!(sweep_expired_in_root_at(&root, now), 1);
        assert!(load_in_root(&root, &provider, 48_884, 48_885).is_some());
        assert!(load_in_root(&root, &provider, 48_884, 48_886).is_none());
        assert_eq!(load_in_root(&root, &provider, 48_884, 48_887), Some(capped));
        assert!(channel.join("48888.json").exists());
    });
}
