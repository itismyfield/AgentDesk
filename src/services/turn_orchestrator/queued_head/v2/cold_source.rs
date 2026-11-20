//! The owner journals only the immutable ticket, its pre-key pin and its selected source.
use super::*;
use crate::services::tui_input::human_frame::history::CAPACITY;
#[cfg(unix)]
use crate::services::tui_input::human_frame::history::v2::{
    Source, native::Ticket as NativeTicket,
};
#[cfg(unix)]
use crate::services::turn_orchestrator::queued_head::v2_types::cold_source::{
    BindingIntent, PreKeyPin,
};

#[cfg(unix)]
impl Owner {
    pub(crate) fn pin_source(
        &mut self,
        ticket: NativeTicket,
        pin: PreKeyPin,
    ) -> Result<(), String> {
        self.recover()?;
        if let Some(old) = self.source_binding(&ticket)? {
            if old.pin != pin {
                return Err("pre-key pin changed".into());
            }
            return self.flush_source_intent();
        }
        let key: AttemptRef =
            serde_json::from_value(ticket.key.clone()).map_err(|e| e.to_string())?;
        let head = self
            .snapshot
            .head_injection
            .as_ref()
            .ok_or("source pin has no head")?;
        if !key.matches(head)
            || ticket.channel_id != head.original.channel_id.ok_or("source channel missing")?
            || ticket.owning_episode != self.native_intent()?
            || ticket.source.is_some()
            || key.attempt.provider != "claude"
            || !matches!(key.attempt.source, AttemptSource::PreSource { .. })
            || pin.binding_seq == 0
            || pin.session_id.is_empty()
            || pin.owner_root.is_empty()
        {
            return Err("source pin lineage changed".into());
        }
        if self.snapshot.source_bindings.len() >= CAPACITY {
            return Err("source binding debt full; no key permitted".into());
        }
        let mut next = self.snapshot.clone();
        next.source_bindings.push(BindingIntent {
            ticket,
            pin,
            selected: None,
        });
        self.commit(next)?;
        self.flush_source_intent()
    }

    pub(crate) fn source_binding(
        &self,
        ticket: &NativeTicket,
    ) -> Result<Option<BindingIntent>, String> {
        let found = self
            .snapshot
            .source_bindings
            .iter()
            .find(|i| i.ticket.key == ticket.key);
        if found.is_some_and(|i| {
            let mut expected = ticket.clone();
            expected.source = i.ticket.source.clone();
            i.ticket != expected
                || ticket
                    .source
                    .as_ref()
                    .is_some_and(|s| i.selected.as_ref() != Some(s))
        }) {
            return Err("source binding ticket changed".into());
        }
        Ok(found.cloned())
    }

    pub(crate) fn select_source(
        &mut self,
        ticket: &NativeTicket,
        source: &Source,
    ) -> Result<(), String> {
        let intent = self
            .source_binding(ticket)?
            .ok_or("source binding intent missing")?;
        if source.session_id != intent.pin.session_id
            || source.transcript != intent.pin.transcript
            || source.offset != intent.pin.offset
            || intent.pin.file.is_some_and(|file| file != source.file)
            || intent.selected.as_ref().is_some_and(|old| old != source)
        {
            return Err("source binding differs from pre-key pin".into());
        }
        if intent.selected.is_none() {
            source.validate(&ticket.key)?;
        }
        let mut next = self.snapshot.clone();
        let saved = next
            .source_bindings
            .iter_mut()
            .find(|i| i.ticket.key == ticket.key)
            .ok_or("source binding intent disappeared")?;
        saved.selected = Some(source.clone());
        self.commit(next)?;
        self.flush_source_intent()
    }

    fn flush_source_intent(&self) -> Result<(), String> {
        // An adopted identical candidate still needs a durability acknowledgement.
        let status =
            crate::services::discord::runtime_store::durable_publish::retry_published_status(
                &self.path,
                crate::services::discord::runtime_store::AtomicWriteContext::new("queued_input_v2"),
            );
        if status != crate::services::discord::runtime_store::durable_publish::FlushStatus::Flushed
        {
            return Err("source binding owner durability unconfirmed".into());
        }
        Ok(())
    }

    /// Called only after the exact history binding (or exact NotSent) has a durable ACK.
    pub(crate) fn ack_source(
        &mut self,
        ticket: &NativeTicket,
        source: Option<&Source>,
    ) -> Result<(), String> {
        let Some(intent) = self.source_binding(ticket)? else {
            return Ok(());
        };
        if source.is_some() && intent.selected.as_ref() != source {
            return Err("source binding acknowledgement changed".into());
        }
        let mut next = self.snapshot.clone();
        if let Some(source) = source {
            let value = serde_json::to_value(source).map_err(|e| e.to_string())?;
            let key: AttemptRef =
                serde_json::from_value(ticket.key.clone()).map_err(|e| e.to_string())?;
            for input in next
                .head_injection
                .iter_mut()
                .chain(next.retained_inputs.iter_mut().map(|r| &mut r.input))
            {
                if key.matches(input) {
                    if input
                        .resolved_source
                        .as_ref()
                        .is_some_and(|old| *old != value)
                    {
                        return Err("source acknowledgement conflicts with active input".into());
                    }
                    input.resolved_source = Some(value.clone());
                }
            }
            for bind in &mut next.admission_binds {
                if let BindAt::Settled {
                    attempt, source, ..
                } = &mut bind.at
                    && ticket.key["attempt"]["nonce"] == *attempt
                {
                    *source = Some(value.clone());
                }
            }
        }
        next.source_bindings.retain(|i| i.ticket.key != ticket.key);
        self.commit(next)
    }
}
