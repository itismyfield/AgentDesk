//! Queue-prefix selection shared by the automatic queue lane.

use proptest::prelude::*;
use serenity::model::id::{MessageId, UserId};

use crate::services::discord::busy_followup_retry_store::RetryClass;
use crate::services::turn_orchestrator::prefix::{Prefix, select};
use crate::services::turn_orchestrator::{Intervention, InterventionMode};

#[derive(Clone, Copy, Debug)]
enum Class {
    Ready,
    Parked,
}

fn class_of(class: Class) -> RetryClass {
    match class {
        Class::Ready => RetryClass::Ready,
        Class::Parked => RetryClass::Parked,
    }
}

fn queued(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(3),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 1,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("prefix {id}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    }
}

fn classes() -> impl Strategy<Value = Vec<Class>> {
    prop::collection::vec(
        prop_oneof![Just(Class::Ready), Just(Class::Parked)],
        0..12,
    )
}

pub(super) fn run() -> Vec<String> {
    super::exercise(
        "prefix_stops_at_the_first_blocking_input",
        prefix_stops_at_the_first_blocking_input,
    )
    .into_iter()
    .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 80, .. ProptestConfig::default() })]
    fn prefix_stops_at_the_first_blocking_input(items in classes()) {
        let queue: Vec<_> = (1..=items.len() as u64).map(queued).collect();
        let choice = select(&queue, |item| {
            let index = usize::try_from(item.message_id.get() - 1).unwrap_or(0);
            class_of(items[index])
        });
        match items.iter().position(|class| matches!(class, Class::Ready)) {
            None => prop_assert_eq!(choice, Prefix::None),
            Some(index) => prop_assert_eq!(choice, Prefix::Ready { at: index }),
        }
    }
}
