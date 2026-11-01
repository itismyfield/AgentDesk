use poise::serenity_prelude::UserId;

use super::Intervention;

pub(super) fn queued_intervention_request_owner(
    intervention: &Intervention,
    fallback_request_owner: UserId,
) -> UserId {
    if is_queued_human_input(intervention) {
        intervention.author_id
    } else {
        fallback_request_owner
    }
}

/// Human input queued over HTTP carries a synthetic id and its verified author.
fn is_queued_human_input(intervention: &Intervention) -> bool {
    !intervention.author_is_bot
        && intervention.author_id != UserId::new(1)
        && super::is_synthetic_headless_message_id_raw(intervention.message_id.get())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::InterventionMode;
    use poise::serenity_prelude::MessageId;
    use std::time::Instant;

    fn queued_intervention(author_id: u64, text: &str) -> Intervention {
        Intervention {
            author_id: UserId::new(author_id),
            author_is_bot: true,
            message_id: MessageId::new(50_001),
            queued_generation: crate::services::discord::runtime_store::process_generation(),
            source_message_ids: vec![MessageId::new(50_001)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
        }
    }

    #[test]
    fn queued_human_input_keeps_its_verified_author_at_drain() {
        let fallback = UserId::new(7);
        let mut human = queued_intervention(343, "status?");
        human.author_is_bot = false;
        human.message_id = MessageId::new(9_100_000_000_000_000_123);
        let mut system_headless = human.clone();
        system_headless.author_id = UserId::new(1);
        let mut discord_message = human.clone();
        discord_message.message_id = MessageId::new(1_500_000_000_000_000_000);
        let owners = [human, system_headless, discord_message]
            .map(|queued| queued_intervention_request_owner(&queued, fallback).get());
        assert_eq!(owners, [343, 7, 7]);
    }
}
