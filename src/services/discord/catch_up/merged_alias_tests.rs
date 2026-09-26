//! A merged head's durable alias settles its absorbed ids only when that same
//! episode (primary, turn nonce) is on the completed-turn ledger.

use super::absorbed_active_tests::absorb_and_claim;
use super::*;
use crate::services::discord::catch_up::settled_ledger_consult;
use crate::services::discord::recovery_known_ids::{
    RecoveryKnownIdArm, recovery_known_arms_and_ids,
};
use crate::services::provider::CancelToken;

fn history(channel_id: ChannelId, ids: [MessageId; 3]) -> Vec<serenity::Message> {
    let [checkpoint, h, p] = ids;
    vec![
        own_reply(channel_id, checkpoint),
        human(channel_id, h),
        human(channel_id, p),
    ]
}

/// A process restarted at `checkpoint` over the same runtime root.
async fn restarted(channel_id: ChannelId, checkpoint: MessageId) -> Arc<SharedData> {
    let shared = discord::make_shared_data_for_tests();
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allow_all_users = true;
    }
    shared.last_message_ids.insert(channel_id, checkpoint.get());
    shared
}

fn append_episode(fx: &Fixture, channel_id: ChannelId, primary: MessageId, nonce: &str) {
    let (provider, channel) = (&fx.provider, channel_id.get());
    completed_turn_ledger::append_completed_episode(provider, channel, primary.get(), Some(nonce));
}

/// P's episode absorbed H and was delivered, so the next scan settles H with no
/// enqueue and advances; a restart agrees only while the ledger survives.
async fn assert_restart_after_delivered_episode(channel_id: ChannelId, ledger_kept: bool) {
    let fx = Fixture::new().await;
    let (checkpoint, h, p) = (id(1, 600), id(2, 150), id(3, 120));
    fx.seed_checkpoint(channel_id, checkpoint);
    let nonce = absorb_and_claim(&fx.shared, &fx.provider, channel_id, &[h], p).await;
    let ledger = completed_turn_ledger::read_ledger(&fx.provider, channel_id.get());
    let aliased = ledger.map(|ledger| ledger.absorbed_by_episode(p.get(), &nonce));
    assert_eq!(
        aliased,
        Some(vec![h.get()]),
        "alias landed before the claim returned"
    );
    append_episode(&fx, channel_id, p, &nonce);
    discord::mailbox_finish_turn(&fx.shared, &fx.provider, channel_id).await;
    let history = history(channel_id, [checkpoint, h, p]);

    let api = StrictApi::new(&fx.shared).with_history(channel_id, history.clone());
    fx.sweep(&api).await;
    assert_eq!(api.enqueue_log(), Vec::<EnqueueRecord>::new());
    assert_eq!(fx.surfaces(channel_id), (Some(p.get()), Some(p.get())));
    assert_eq!(fx.pending(channel_id), None);

    // A crash before the advance persisted: the restart rescans H and P.
    fx.seed_checkpoint(channel_id, checkpoint);
    if !ledger_kept {
        let path = completed_turn_ledger::ledger_path(&fx.provider, channel_id.get());
        std::fs::remove_file(path.expect("ledger path")).expect("remove ledger");
    }
    let shared = restarted(channel_id, checkpoint).await;
    let api = StrictApi::new(&shared).with_history(channel_id, history);
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &fx.provider)).await;
    assert_phase1_read(&api, after(checkpoint), &[h, p]);
    if !ledger_kept {
        // Negative control: without the ledger evidence both ids are recovered.
        let recovered = accepted(&api);
        let both = recovered.contains(&h.get()) && recovered.contains(&p.get());
        assert!(both, "{:?}", api.enqueue_log());
        return;
    }
    assert_eq!(
        api.enqueue_log(),
        Vec::<EnqueueRecord>::new(),
        "restart re-runs nothing"
    );
    let memory = shared.last_message_ids.get(&channel_id).map(|id| *id);
    assert_eq!(memory, Some(p.get()));
}

#[tokio::test(flavor = "current_thread")]
async fn t_s1_a_delivered_episode_settles_what_it_absorbed() {
    assert_restart_after_delivered_episode(ChannelId::new(4_603_552), true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn t_s1_a_restart_without_the_ledger_recovers_h_and_p() {
    assert_restart_after_delivered_episode(ChannelId::new(4_603_560), false).await;
}

/// P's delivered episode n1 row, appended before or after the alias, cannot
/// certify the undelivered episode n2 that absorbed H, so H is recovered.
async fn assert_other_episode_row_does_not_settle(channel_id: ChannelId, delayed: bool) {
    let fx = Fixture::new().await;
    let (checkpoint, h, p) = (id(1, 600), id(2, 150), id(3, 120));
    fx.seed_checkpoint(channel_id, checkpoint);
    let token = Arc::new(CancelToken::new());
    let first = token.turn_nonce().expect("live nonce").to_owned();
    let owner = serenity::UserId::new(HUMAN_ID);
    assert!(discord::mailbox_try_start_turn(&fx.shared, channel_id, token, owner, p).await);
    if !delayed {
        append_episode(&fx, channel_id, p, &first);
    }
    discord::mailbox_finish_turn(&fx.shared, &fx.provider, channel_id).await;
    let second = absorb_and_claim(&fx.shared, &fx.provider, channel_id, &[h], p).await;
    assert_ne!(first, second);
    if delayed {
        append_episode(&fx, channel_id, p, &first);
    }
    discord::mailbox_finish_turn(&fx.shared, &fx.provider, channel_id).await;

    let api = StrictApi::new(&fx.shared)
        .with_history(channel_id, history(channel_id, [checkpoint, h, p]));
    fx.sweep(&api).await;

    assert!(accepted(&api).contains(&h.get()), "{:?}", api.enqueue_log());
}

#[tokio::test(flavor = "current_thread")]
async fn t_s2_an_earlier_delivered_episode_does_not_settle_a_later_absorption() {
    assert_other_episode_row_does_not_settle(ChannelId::new(4_603_553), false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn t_s3_a_delayed_append_of_an_earlier_episode_does_not_settle_h() {
    assert_other_episode_row_does_not_settle(ChannelId::new(4_603_554), true).await;
}

/// The alias lives on the claim channel's ledger and a split delivery on
/// another's, so H is duplicated, never settled.
#[tokio::test(flavor = "current_thread")]
async fn t_s7_a_split_channel_delivery_duplicates_but_never_settles() {
    let fx = Fixture::new().await;
    let (channel_id, delivery) = (ChannelId::new(4_603_555), ChannelId::new(4_603_556));
    let (checkpoint, h, p) = (id(1, 600), id(2, 150), id(3, 120));
    fx.seed_checkpoint(channel_id, checkpoint);
    let nonce = absorb_and_claim(&fx.shared, &fx.provider, channel_id, &[h], p).await;
    append_episode(&fx, delivery, p, &nonce);
    discord::mailbox_finish_turn(&fx.shared, &fx.provider, channel_id).await;

    let api = StrictApi::new(&fx.shared)
        .with_history(channel_id, history(channel_id, [checkpoint, h, p]));
    fx.sweep(&api).await;

    assert!(accepted(&api).contains(&h.get()), "{:?}", api.enqueue_log());
    let ledger = completed_turn_ledger::read_ledger(&fx.provider, delivery.get());
    let settled = ledger.expect("delivery ledger").settled_ids();
    assert!(
        !settled.contains(&h.get()),
        "the delivery channel never settles H"
    );
}

/// A restored episode has no in-memory absorbed set; its durable alias restores
/// H as `AbsorbedActiveTurn`, so H is neither re-run nor leapt.
#[tokio::test(flavor = "current_thread")]
async fn t_s8_a_restored_episode_holds_its_durably_absorbed_ids() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_557);
    let (checkpoint, h, p) = (id(1, 600), id(2, 150), id(3, 120));
    fx.seed_checkpoint(channel_id, checkpoint);
    let nonce = absorb_and_claim(&fx.shared, &fx.provider, channel_id, &[h], p).await;

    let shared = restarted(channel_id, checkpoint).await;
    let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some(nonce)));
    let owner = serenity::UserId::new(HUMAN_ID);
    shared
        .mailbox(channel_id)
        .restore_active_turn(token, owner, p)
        .await;
    let snapshot = discord::mailbox_snapshot(&shared, channel_id).await;
    let (mut arms, mut known) = (recovery_known_arms_and_ids(&snapshot).0, HashSet::new());
    let ledger = settled_ledger_consult::read(&fx.provider, channel_id);
    ledger.settle(&snapshot, &mut arms, &mut known);
    let arm = arms.get(&h.get()).copied();
    assert_eq!(arm, Some(RecoveryKnownIdArm::AbsorbedActiveTurn));
    assert!(known.contains(&h.get()));

    let api =
        StrictApi::new(&shared).with_history(channel_id, history(channel_id, [checkpoint, h, p]));
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &fx.provider)).await;
    assert!(
        !accepted(&api).contains(&h.get()),
        "{:?}",
        api.enqueue_log()
    );
    let memory = shared.last_message_ids.get(&channel_id).map(|id| *id);
    assert_eq!(memory, Some(checkpoint.get()), "H is not leapt");
}

/// A legacy nonce-less token or a claim that absorbed nothing writes no alias.
#[tokio::test(flavor = "current_thread")]
async fn a_claim_without_nonce_or_absorption_writes_no_alias() {
    let fx = Fixture::new().await;
    let (legacy, plain) = (ChannelId::new(4_603_558), ChannelId::new(4_603_559));
    let (h, p) = (id(2, 150), id(3, 120));
    let owner = serenity::UserId::new(HUMAN_ID);
    super::absorbed_active_tests::merge_and_take(&fx.shared, &fx.provider, legacy, &[h], p).await;
    let token = Arc::new(CancelToken::from_persisted_turn_nonce(None));
    assert!(discord::mailbox_try_start_turn(&fx.shared, legacy, token, owner, p).await);
    let token = Arc::new(CancelToken::new());
    assert!(discord::mailbox_try_start_turn(&fx.shared, plain, token, owner, p).await);

    for channel_id in [legacy, plain] {
        let ledger = completed_turn_ledger::read_ledger(&fx.provider, channel_id.get());
        let aliases = ledger
            .map(|ledger| ledger.merged_aliases)
            .unwrap_or_default();
        assert!(aliases.is_empty(), "{channel_id}: {aliases:?}");
    }
}
