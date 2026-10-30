use super::super::{ChannelMailboxHandle, ChannelMailboxMsg, HeadMsg};
use super::*;

type Action = Box<dyn FnOnce(&mut Option<Owner>) + Send>;
pub(crate) struct Message(Action);
impl Message {
    pub(crate) fn run(self, owner: &mut Option<Owner>) {
        (self.0)(owner);
    }
    pub(crate) fn refuse(self) {
        drop(self);
    }
}

impl ChannelMailboxHandle {
    pub(crate) async fn v2_adopt(&self, path: PathBuf, namespace: String) -> Result<(), String> {
        self.v2_request(move |slot| {
            *slot = Some(Owner::adopt(path, namespace)?);
            Ok(())
        })
        .await
    }

    /// Adopts the channel's queue file in one actor turn unless its owner already holds it; an
    /// owner of another file or namespace refuses.
    pub(crate) async fn v2_ensure(&self, path: PathBuf, namespace: String) -> Result<(), String> {
        self.v2_request(move |slot| match slot {
            Some(owner)
                if owner.path == path
                    && owner.snapshot.namespace.as_deref() == Some(namespace.as_str()) =>
            {
                Ok(())
            }
            Some(_) => Err("another queue file's owner holds this channel".into()),
            None => {
                *slot = Some(Owner::adopt(path, namespace)?);
                Ok(())
            }
        })
        .await
    }

    pub(crate) async fn v2_request<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Option<Owner>) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        self.request(move |reply| {
            let msg = Message(Box::new(move |owner| {
                let _ = reply.send(f(owner));
            }));
            ChannelMailboxMsg::Injection(super::super::super::injected_inputs::InjectionMsg::Head(
                HeadMsg::V2(msg),
            ))
        })
        .await
        .map_err(|_| "mailbox unreachable".to_string())?
    }
}
