use super::*;

pub const TOKEN: &str = "queued-head-s2a";
pub(super) const GENERATION: u64 = 5;

pub fn root() -> crate::config::TestRuntimeRootGuard {
    crate::config::TestRuntimeRootGuard::new()
}

pub fn run(work: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(work);
}

pub fn ctx() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, TOKEN, None)
}

pub fn human(id: u64, author: u64, text: &str) -> Intervention {
    Intervention {
        author_id: UserId::new(author),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: GENERATION,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: vec![SourceMessageQueuedGeneration::user_instruction(
            MessageId::new(id),
            GENERATION,
        )],
        source_text_segments: Vec::new(),
        text: text.into(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: true,
        pending_uploads: Vec::new(),
    }
}

pub fn path(channel: ChannelId) -> std::path::PathBuf {
    pending_queue_file_path(&ProviderKind::Claude, TOKEN, channel).unwrap()
}

pub fn bytes(channel: ChannelId) -> Vec<u8> {
    std::fs::read(path(channel)).unwrap()
}

pub(super) async fn reserve(
    handle: &ChannelMailboxHandle,
    head: &Intervention,
) -> claim::HeadTicket {
    match handle
        .claim_queued_head(QueueIdentity::of(head), ctx())
        .await
    {
        QueuedHeadClaim::Reserved(ticket) => ticket,
        _ => panic!("expected durable head reservation"),
    }
}

pub async fn reserve_unknown(handle: &ChannelMailboxHandle, head: &Intervention) {
    let ticket = reserve(handle, head).await;
    assert!(matches!(
        handle
            .settle_queued_head(ticket, HeadOutcome::Unconfirmed, ctx())
            .await,
        HeadSettle::Unknown
    ));
}

pub async fn queue(handle: &ChannelMailboxHandle) -> Vec<(u64, String)> {
    handle
        .snapshot()
        .await
        .intervention_queue
        .into_iter()
        .map(|item| (item.message_id.get(), item.text))
        .collect()
}

impl ChannelMailboxRegistry {
    pub fn remove_fixture_for_test(&self, channel: ChannelId) {
        if let Some((_, handle)) = self.handles.remove(&channel) {
            GLOBAL_CHANNEL_MAILBOXES.remove_if(&channel, |_, h| h.same_actor(&handle));
        }
    }
}
