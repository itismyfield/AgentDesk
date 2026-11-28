use std::sync::Arc;

use super::{ResolvedIdentity, TurnViewDelivery, TurnViewReconciler, TurnViewTarget};
use poise::serenity_prelude as serenity;

pub(super) enum Route {
    Record(Arc<serenity::Http>),
    Http(Arc<serenity::Http>),
}

impl Route {
    pub(super) fn http(&self) -> &Arc<serenity::Http> {
        match self {
            Self::Record(http) | Self::Http(http) => http,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct TestReactionOp {
    pub(in crate::services::discord) target: TurnViewTarget,
    pub(in crate::services::discord) emoji: char,
    pub(in crate::services::discord) add: bool,
    pub(in crate::services::discord) identity: String,
}

impl TurnViewReconciler {
    pub(in crate::services::discord) fn record_test_reactions(&self, http: Arc<serenity::Http>) {
        assert!(self.test_reaction_route.set(Route::Record(http)).is_ok());
    }

    pub(in crate::services::discord) fn use_test_reaction_http(&self, http: Arc<serenity::Http>) {
        assert!(self.test_reaction_route.set(Route::Http(http)).is_ok());
    }

    pub(in crate::services::discord) fn ops(&self) -> Vec<TestReactionOp> {
        self.ops.lock().expect("turn view test op lock").clone()
    }

    pub(super) fn record_reaction_op(
        &self,
        target: TurnViewTarget,
        emoji: char,
        add: bool,
        identity: &ResolvedIdentity,
    ) {
        self.ops
            .lock()
            .expect("turn view test op lock")
            .push(TestReactionOp {
                target,
                emoji,
                add,
                identity: identity.label.clone(),
            });
    }
}

pub(super) async fn deliver_http(
    http: &serenity::Http,
    target: TurnViewTarget,
    emoji: char,
    add: bool,
) -> TurnViewDelivery {
    let reaction = serenity::ReactionType::Unicode(emoji.to_string());
    let result = if add {
        target
            .channel_id
            .create_reaction(http, target.message_id, reaction)
            .await
    } else {
        target
            .channel_id
            .delete_reaction(http, target.message_id, None, reaction)
            .await
    };
    match result {
        Ok(()) => TurnViewDelivery::Delivered,
        Err(serenity::Error::Http(error)) => TurnViewDelivery::from_reaction_error_status(
            error.status_code().map(|status| status.as_u16()),
        ),
        Err(_) => TurnViewDelivery::Failed,
    }
}
