//! A source refinement and its durability ACK are separate from a native membership receipt.
use super::native::Ticket;
use super::*;

impl Registry {
    pub fn bind_native_source(
        &self,
        namespace: &str,
        ticket: &Ticket,
        source: &Source,
    ) -> Result<(), String> {
        self.try_control_transaction(|state| {
            let (registered, _) = state
                .locate(namespace, &ticket.key)
                .ok_or("source registration missing")?;
            let native = registered
                .native_settlement
                .as_ref()
                .ok_or("source native ticket missing")?;
            native.validate(registered)?;
            let mut expected = ticket.clone();
            expected.source = registered.source.clone();
            if expected != native.ticket || ticket.source.as_ref().is_some_and(|old| old != source)
            {
                return Err("source native ticket changed".into());
            }
            // A successful bind whose ACK was lost remains acknowledgeable after terminal/control.
            if let Some(old) = &registered.source {
                return (old == source)
                    .then_some(())
                    .ok_or("source already bound differently".into());
            }
            if native.outcome.is_some()
                || native.cleanup_complete
                || native.extra.contains_key("control_intent")
                || native.extra.get("not_sent") == Some(&Value::Bool(true))
            {
                return Err("terminal ticket cannot acquire a new source".into());
            }
            source.validate(&ticket.key)?;
            if let Some(at) = state
                .planned
                .iter()
                .position(|r| r.namespace == namespace && r.key == ticket.key)
            {
                let mut registration = state.planned.remove(at);
                registration.source = Some(source.clone());
                registration
                    .native_settlement
                    .as_mut()
                    .ok_or("source ticket disappeared")?
                    .ticket
                    .source = Some(source.clone());
                state.live.push(Bound {
                    entry: registration.frame.bind(&ticket.key, source),
                    registration,
                });
            } else {
                let passive = state
                    .passive
                    .iter_mut()
                    .find(|p| {
                        p.registration
                            .as_ref()
                            .is_some_and(|r| r.namespace == namespace && r.key == ticket.key)
                    })
                    .ok_or("source registration disappeared")?;
                let registration = passive
                    .registration
                    .as_mut()
                    .ok_or("source legacy registration")?;
                registration.source = Some(source.clone());
                registration
                    .native_settlement
                    .as_mut()
                    .ok_or("source ticket disappeared")?
                    .ticket
                    .source = Some(source.clone());
                passive.entry = Some(registration.frame.bind(&ticket.key, source));
                passive.encoded = None;
            }
            Ok(())
        })
    }
}
