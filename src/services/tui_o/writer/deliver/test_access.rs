//! Upper-crate tests read the writer's suspension and drive a re-post turn through these;
//! the field and the turn stay crate-private in a default build.
use super::super::suspension::Suspension;
use super::{AlarmSink, ChannelWriter, DeliveryLease, DiscordPort};

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> ChannelWriter<P, L, A> {
    /// A handle on the suspension this writer checks before it delivers or re-posts.
    pub fn suspension_for_test(&self) -> Suspension {
        self.suspension.clone()
    }

    /// One re-post turn, as the actor runs it after a poll.
    pub async fn repost_turn_for_test(&mut self) {
        self.repost_turn().await;
    }
}
