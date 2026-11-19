//! O refines cold source tickets using pre-key custody and that execution's immutable binding.
use super::*;
use crate::services::claude_tui::source_verify::{
    Observation as FileObservation, PinJudgment, is_top_level_transcript, judge_pin,
    observe_transcript,
};
use crate::services::tui_input::human_frame::history::v2::Source;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::writer::binding::{BindingEvent, ChannelBindingLog};
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context};
use crate::services::turn_orchestrator::{PreKeyPin, QueuePersistenceContext};
use std::io::{Read, Seek, SeekFrom};

fn persistence(shared: &SharedData) -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None)
}

pub(in crate::services::discord) async fn pin_source(
    shared: &Arc<SharedData>,
    namespace: &str,
    ticket: &Ticket,
) -> Result<(), String> {
    if ticket.source.is_some() {
        return Ok(());
    }
    if namespace != format!("claude/{}", shared.token_hash)
        || ticket.key["attempt"]["provider"] != "claude"
    {
        return Err("cold source domain changed".into());
    }
    let events = ChannelBindingLog::new(ticket.channel_id, ShadowProvider::Claude)
        .binding_history(ticket.channel_id)?;
    let target = &ticket.key["attempt"]["target"];
    let event = events
        .iter()
        .rev()
        .find(|e| {
            e.tmux_session == target["owner"] && matches!(e.record, BindingRecord::Bound { .. })
        })
        .ok_or("cold source binding not recorded")?;
    if event.execution_nonce != target["execution_nonce"] {
        return Err("cold source execution changed".into());
    }
    let (session, path) = match &event.record {
        BindingRecord::Bound {
            new:
                BindingTarget::Pending {
                    payload_session_id,
                    payload_transcript_path,
                },
            ..
        } => (payload_session_id.clone(), payload_transcript_path.clone()),
        BindingRecord::Bound {
            new: BindingTarget::Source(source),
            ..
        } => (source.session_id.clone(), source.path.clone()),
        _ => return Err("cold source binding unavailable".into()),
    };
    let context = binding_context::execution_context("claude", &event.execution_nonce)?;
    if context.tmux_session != event.tmux_session
        || context.channel_id != Some(ticket.channel_id)
        || !context
            .provider_root
            .as_ref()
            .is_some_and(|root| is_top_level_transcript(root, &path, &session))
    {
        return Err("cold source launch path changed".into());
    }
    let expected = &ticket.key["attempt"]["source"];
    if expected["kind"] != "pre_source"
        || expected["expected_session_id"]
            .as_str()
            .is_some_and(|s| s != session)
        || expected["expected_transcript"]
            .as_str()
            .is_some_and(|s| std::path::Path::new(s) != path)
    {
        return Err("cold source expectation changed".into());
    }
    let (offset, file) = boundary(&path)?;
    if let Some([dev, ino]) = file.filter(|_| offset != 0) {
        verify_source(&SourceId {
            session_id: session.clone(),
            path: path.clone(),
            dev,
            ino,
        })?;
    }
    let pin = PreKeyPin {
        binding_seq: event.seq,
        session_id: session,
        transcript: path,
        offset,
        file,
        owner_root: context.owner_runtime_root,
    };
    let mailbox = shared.mailbox(ChannelId::new(ticket.channel_id));
    let ticket = ticket.clone();
    mailbox
        .v2_request(move |slot| {
            slot.as_mut()
                .ok_or("cold source owner missing")?
                .pin_source(ticket, pin)
        })
        .await
}

fn boundary(path: &std::path::Path) -> Result<(u64, Option<[u64; 2]>), String> {
    use std::os::unix::fs::MetadataExt;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, None)),
        Err(e) => return Err(format!("cold source boundary unreadable: {e}")),
    };
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("cold source is not a regular file".into());
    }
    if meta.len() != 0 {
        file.seek(SeekFrom::Start(meta.len() - 1))
            .map_err(|e| e.to_string())?;
        let mut byte = [0];
        file.read_exact(&mut byte).map_err(|e| e.to_string())?;
        if byte != [b'\n'] {
            return Err("cold source has an incomplete pre-key record".into());
        }
    }
    if file.metadata().map_err(|e| e.to_string())?.len() != meta.len() {
        return Err("cold source boundary moved".into());
    }
    Ok((meta.len(), Some([meta.dev(), meta.ino()])))
}

pub(in crate::services::discord) async fn release_unkeyed_source(
    shared: &Arc<SharedData>,
    history: &Registry,
    namespace: &str,
    ticket: &Ticket,
) -> Result<(), String> {
    let current = history.native_ticket(namespace, &ticket.key)?;
    let mut expected = ticket.clone();
    expected.source = current.source.clone();
    let saved = history.native_settlement(namespace, &current)?;
    if expected != current
        || saved.extra.get("not_sent") != Some(&serde_json::Value::Bool(true))
        || saved.outcome.is_some()
        || saved.extra.get("any_key") == Some(&serde_json::Value::Bool(true))
    {
        return Err("cold source no-key proof missing".into());
    }
    let ticket = ticket.clone();
    shared
        .mailbox(ChannelId::new(ticket.channel_id))
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("cold source owner missing")?;
            owner.ack_source(&ticket, None)
        })
        .await
}

impl Port {
    pub(super) async fn retry_source_bindings(&self) {
        let Ok((live, boot)) = self.custody().await else {
            return;
        };
        let mut inputs: Vec<_> = live
            .into_iter()
            .map(|i| (i.namespace, i.ticket, i.history))
            .collect();
        if let Ok(history) = Registry::runtime() {
            inputs.extend(boot.into_iter().map(|i| {
                let (ns, ticket) = i.identity();
                (ns.to_owned(), ticket.clone(), history.clone())
            }));
        }
        for (namespace, ticket, history) in inputs {
            if let Err(error) = self.bind_source(&namespace, &ticket, &history).await {
                tracing::debug!(channel_id=self.channel, %error, "cold source binding debt pending");
            }
        }
    }

    async fn bind_source(
        &self,
        namespace: &str,
        ticket: &Ticket,
        history: &Registry,
    ) -> Result<(), String> {
        if ticket.channel_id != self.channel
            || namespace != format!("claude/{}", self.shared.token_hash)
            || ticket.key["attempt"]["provider"] != "claude"
        {
            return Err("cold source custody domain changed".into());
        }
        let mailbox = self.shared.mailbox(ChannelId::new(self.channel));
        // Native custody can outlive the last queue head; opening the owner starts no input.
        mailbox
            .v2_ensure_channel(&persistence(&self.shared), ChannelId::new(self.channel))
            .await?;
        let wanted = ticket.clone();
        let Some(intent) = mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().ok_or("cold source owner not adopted")?;
                owner.recover()?;
                owner.source_binding(&wanted)
            })
            .await?
        else {
            return Ok(());
        };
        let saved = history
            .native_settlement(namespace, &history.native_ticket(namespace, &ticket.key)?)?;
        if saved.extra.get("not_sent") == Some(&serde_json::Value::Bool(true)) {
            return release_unkeyed_source(&self.shared, history, namespace, &intent.ticket).await;
        }
        if saved.ticket.source.is_none()
            && (saved.outcome.is_some()
                || saved.cleanup_complete
                || saved.extra.contains_key("control_intent"))
        {
            return Err("terminal cold ticket cannot acquire a source".into());
        }
        let source = match intent.selected {
            Some(source) => source,
            None => {
                self.resolve_pending(&intent.ticket, &intent.pin)?;
                let events = self.bindings.binding_events_since(self.channel, 0)?;
                let source = bound_source(&events, &intent.ticket, &intent.pin)?;
                verify_source(&source)?;
                Source {
                    target: intent.ticket.key["attempt"]["target"].clone(),
                    provider: "claude".into(),
                    session_id: source.session_id,
                    transcript: source.path,
                    file: [source.dev, source.ino],
                    offset: intent.pin.offset,
                    binding_watermark: intent.ticket.key["attempt"]["source"]["binding_watermark"]
                        .as_str()
                        .map(str::to_owned),
                }
            }
        };
        let wanted = intent.ticket.clone();
        let selected = source.clone();
        mailbox
            .v2_request(move |slot| {
                slot.as_mut()
                    .ok_or("cold source owner missing")?
                    .select_source(&wanted, &selected)
            })
            .await?;
        // Keep the owner intent until the history transaction has confirmed publication durability.
        history.bind_native_source(namespace, &intent.ticket, &source)?;
        let wanted = intent.ticket;
        mailbox
            .v2_request(move |slot| {
                slot.as_mut()
                    .ok_or("cold source owner missing")?
                    .ack_source(&wanted, Some(&source))
            })
            .await
    }

    fn resolve_pending(&self, ticket: &Ticket, pin: &PreKeyPin) -> Result<(), String> {
        let events = self.bindings.binding_events_since(self.channel, 0)?;
        if bound_source(&events, ticket, pin).is_ok() {
            return Ok(());
        }
        let target = &ticket.key["attempt"]["target"];
        let logical = target["owner"].as_str().ok_or("cold source owner absent")?;
        let nonce = target["execution_nonce"]
            .as_str()
            .ok_or("cold source nonce absent")?;
        crate::services::tmux_common::with_tmux_source_authority(logical, |authority| {
            if binding_context::observe_spawn_nonce_marker(logical)
                != binding_context::SpawnNonceMarker::Known(nonce.into())
            {
                return Err("cold pending execution superseded".into());
            }
            let context = binding_context::execution_context("claude", nonce)?;
            if context.owner_runtime_root != pin.owner_root
                || context.tmux_session != logical
                || context.channel_id != Some(self.channel)
                || !context
                    .provider_root
                    .as_ref()
                    .is_some_and(|r| is_top_level_transcript(r, &pin.transcript, &pin.session_id))
            {
                return Err("cold pending launch authority changed".into());
            }
            let events = self.bindings.binding_events_since(self.channel, 0)?;
            let pending = events
                .iter()
                .find(|e| e.seq == pin.binding_seq)
                .ok_or("cold pending event missing")?;
            if pending.execution_nonce != nonce || pending.tmux_session != logical
                || events.iter().any(|e| e.tmux_session == logical && e.seq > pin.binding_seq && !matches!(e.record, BindingRecord::Resolved { resolves_seq, .. } if resolves_seq == pin.binding_seq))
                || !matches!(&pending.record, BindingRecord::Bound { new: BindingTarget::Pending { payload_session_id, payload_transcript_path }, .. } if *payload_session_id == pin.session_id && *payload_transcript_path == pin.transcript) {
                return Err("cold pending binding moved".into());
            }
            let opened = observe_transcript(&pin.transcript);
            let prior = pin.file.map(|file| dedupe::binding_events::SourceId {
                session_id: pin.session_id.clone(),
                path: pin.transcript.clone(),
                dev: file[0],
                ino: file[1],
            });
            let PinJudgment::Verified(source) = judge_pin(
                prior.as_ref(),
                Some(pending.committed_at),
                &pin.session_id,
                &pin.transcript,
                FileObservation::Opened(opened.as_ref()),
            ) else {
                return Err("cold pending exact transcript not verified".into());
            };
            let binding = dedupe::TuiRuntimeBinding {
                runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                output_path: pin.transcript.display().to_string(),
                session_id: Some(pin.session_id.clone()),
                last_offset: pin.offset,
                relay_output_path: None,
                input_fifo_path: None,
                relay_last_offset: None,
            };
            let published = dedupe::pane_registration::register_claude_pane_under_source_authority(
                authority,
                self.channel,
                binding,
                dedupe::Record::Exact(source),
            );
            if !published.is_some_and(dedupe::Persisted::published) {
                return Err("cold pending publication unconfirmed".into());
            }
            Ok(())
        })
    }
}

fn verify_source(source: &SourceId) -> Result<(), String> {
    let opened = observe_transcript(&source.path);
    match judge_pin(
        Some(source),
        None,
        &source.session_id,
        &source.path,
        FileObservation::Opened(opened.as_ref()),
    ) {
        PinJudgment::Verified(checked) if checked == *source => Ok(()),
        _ => Err("cold source exact transcript not verified".into()),
    }
}

fn bound_source(
    events: &[BindingEvent],
    ticket: &Ticket,
    pin: &PreKeyPin,
) -> Result<SourceId, String> {
    let target = &ticket.key["attempt"]["target"];
    let bound = events
        .iter()
        .find(|e| e.seq == pin.binding_seq)
        .ok_or("cold source event missing")?;
    if bound.channel_id != ticket.channel_id
        || bound.provider != ShadowProvider::Claude
        || target["owner"] != bound.tmux_session
        || target["execution_nonce"] != bound.execution_nonce
    {
        return Err("cold source binding lineage changed".into());
    }
    let source = match &bound.record {
        BindingRecord::Bound {
            new: BindingTarget::Source(source),
            ..
        } => Some(source),
        BindingRecord::Bound {
            new:
                BindingTarget::Pending {
                    payload_session_id,
                    payload_transcript_path,
                },
            ..
        } if *payload_session_id == pin.session_id
            && *payload_transcript_path == pin.transcript =>
        {
            events.iter().find_map(|e| match &e.record {
                BindingRecord::Resolved {
                    resolves_seq,
                    source,
                } if *resolves_seq == bound.seq
                    && e.channel_id == bound.channel_id
                    && e.provider == bound.provider
                    && e.tmux_session == bound.tmux_session
                    && e.execution_nonce == bound.execution_nonce =>
                {
                    Some(source)
                }
                _ => None,
            })
        }
        _ => None,
    }
    .ok_or("cold source still pending")?;
    if source.session_id != pin.session_id
        || source.path != pin.transcript
        || pin.file.is_some_and(|f| f != [source.dev, source.ino])
    {
        return Err("cold source physical identity changed".into());
    }
    Ok(source.clone())
}

#[cfg(test)]
#[path = "native_binding_tests.rs"]
mod tests;
#[cfg(test)]
pub(in crate::services::discord) use tests::BindingRoot;
#[cfg(test)]
pub(in crate::services::discord::turn_finalizer) use tests::key_witness::bind_before_key_witness;
