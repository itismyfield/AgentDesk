use super::super::{ChannelMailboxHandle, ChannelMailboxMsg, HeadMsg};
use super::*;

type Action = Box<dyn FnOnce(&mut Option<Owner>, bool) + Send>;
pub(crate) struct Message(Action);
impl Message {
    /// `v1_holds`: the legacy queue or its dispatch reservation still holds an input.
    pub(crate) fn run(self, owner: &mut Option<Owner>, v1_holds: bool) {
        (self.0)(owner, v1_holds);
    }
    pub(crate) fn refuse(self) {
        drop(self);
    }
}

impl ChannelMailboxHandle {
    #[cfg(test)]
    pub(crate) async fn v2_adopt(&self, path: PathBuf, namespace: String) -> Result<(), String> {
        self.v2_request(move |slot| {
            *slot = Some(Owner::adopt(path, namespace)?);
            Ok(())
        })
        .await
    }

    /// Adopts the channel's queue file in one actor turn unless its owner already holds it; an
    /// owner of another file or namespace refuses, and a legacy backlog is left to drain first.
    pub(crate) async fn v2_ensure(&self, path: PathBuf, namespace: String) -> Result<(), String> {
        self.v2_turn(move |slot, v1_holds| match slot {
            Some(owner)
                if owner.path == path
                    && owner.snapshot.namespace.as_deref() == Some(namespace.as_str()) =>
            {
                Ok(())
            }
            Some(_) => Err("another queue file's owner holds this channel".into()),
            None if v1_holds => Ok(()),
            None => {
                *slot = Some(Owner::adopt(path, namespace)?);
                Ok(())
            }
        })
        .await
    }

    /// Adopts the channel's own queue file under its bot's namespace unless its owner holds it.
    pub(crate) async fn v2_ensure_channel(
        &self,
        persistence: &crate::services::turn_orchestrator::QueuePersistenceContext,
        channel: poise::serenity_prelude::ChannelId,
    ) -> Result<(), String> {
        let (path, namespace) = v2_queue(persistence, channel)?;
        self.v2_ensure(path, namespace).await
    }

    pub(crate) async fn v2_request<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Option<Owner>) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        self.v2_turn(move |owner, _| f(owner)).await
    }

    async fn v2_turn<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Option<Owner>, bool) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        self.request(move |reply| {
            let msg = Message(Box::new(move |owner, v1_holds| {
                let _ = reply.send(f(owner, v1_holds));
            }));
            ChannelMailboxMsg::Injection(super::super::super::injected_inputs::InjectionMsg::Head(
                HeadMsg::V2(msg),
            ))
        })
        .await
        .map_err(|_| "mailbox unreachable".to_string())?
    }
}

/// The channel's own queue file and its bot's namespace, as the owner adopts them.
pub(crate) fn v2_queue(
    persistence: &crate::services::turn_orchestrator::QueuePersistenceContext,
    channel: poise::serenity_prelude::ChannelId,
) -> Result<(std::path::PathBuf, String), String> {
    let (provider, token) = (&persistence.provider, persistence.token_hash.as_str());
    let path =
        crate::services::turn_orchestrator::pending_queue_persistence::pending_queue_file_path(
            provider, token, channel,
        );
    let namespace = format!("{}/{token}", provider.as_str());
    Ok((path.ok_or("pending queue root unavailable")?, namespace))
}
