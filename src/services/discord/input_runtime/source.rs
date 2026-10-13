//! Materialized input with positively named Discord provenance.

use std::io;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::services::discord::input_runtime::intake::{PreparedSource, StagedAttachment};
use crate::services::tui_input::blob::BlobPin;
use crate::services::tui_input::ledger::Ledger;
use crate::services::tui_input::rows::Entry;
use crate::services::tui_input::rows::receipt_identity::ReceiptIdentity;

pub(crate) struct Source {
    key: u64,
    identity: ReceiptIdentity,
    input: Value,
    pins: Vec<BlobPin>,
    staged: Option<Vec<StagedAttachment>>,
}

impl Source {
    pub(crate) fn new(
        key: u64,
        identity: ReceiptIdentity,
        mut input: Value,
        pins: Vec<BlobPin>,
    ) -> io::Result<Self> {
        let checked = ReceiptIdentity::new(
            key,
            identity.source_ids.clone(),
            identity.author_id,
            identity.original_channel_id,
            identity.execution_channel_id,
        )?;
        let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "source provenance differs");
        let object = input.as_object_mut().ok_or_else(invalid)?;
        let mut ids = vec![key];
        if let Some(values) = object.get("source_message_ids") {
            for value in values.as_array().ok_or_else(invalid)? {
                let id = value.as_u64().filter(|id| *id != 0).ok_or_else(invalid)?;
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();
        if !object.get("text").is_some_and(Value::is_string)
            || ids != checked.source_ids
            || object
                .get("message_id")
                .is_some_and(|v| v.as_u64() != Some(key))
            || object
                .get("author_id")
                .is_some_and(|v| v.as_u64() != Some(checked.author_id))
            || object
                .get("original_channel_id")
                .is_some_and(|v| v.as_u64() != Some(checked.original_channel_id))
            || object
                .get("execution_channel_id")
                .is_some_and(|v| v.as_u64() != Some(checked.execution_channel_id))
            || object.get("receipt_identity").is_some_and(|v| {
                let identity = serde_json::from_value::<ReceiptIdentity>(v.clone()).ok();
                #[cfg(test)]
                if super::mutant("raw_identity_order") {
                    return identity.as_ref() != Some(&checked);
                }
                identity
                    .and_then(|i| {
                        ReceiptIdentity::new(
                            key,
                            i.source_ids,
                            i.author_id,
                            i.original_channel_id,
                            i.execution_channel_id,
                        )
                        .ok()
                    })
                    .as_ref()
                    != Some(&checked)
            })
        {
            return Err(invalid());
        }
        object.insert("receipt_identity".into(), serde_json::to_value(&checked)?);
        Ok(Self {
            key,
            identity: checked,
            input,
            pins,
            staged: None,
        })
    }

    pub(in crate::services::discord) fn from_prepared(
        prepared: PreparedSource,
    ) -> io::Result<Self> {
        let (key, identity, input, attachments) = prepared.into_parts();
        let mut source = Self::new(key, identity, input, Vec::new())?;
        source.staged = Some(attachments);
        Ok(source)
    }

    pub(super) fn pin_staged(&mut self, ledger: &Ledger) -> io::Result<()> {
        let Some(attachments) = &self.staged else {
            return Ok(());
        };
        let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "unfixed attachment material");
        let references = self
            .input
            .get("attachments")
            .map(|value| value.as_array().ok_or_else(invalid))
            .transpose()?;
        if references.map_or(0, Vec::len) != attachments.len() {
            return Err(invalid());
        }
        for (index, attachment) in attachments.iter().enumerate() {
            let reference = references
                .and_then(|values| values.get(index))
                .ok_or_else(invalid)?;
            if attachment.filename.trim().is_empty()
                || attachment.url.trim().is_empty()
                || reference["filename"].as_str() != Some(attachment.filename.as_str())
                || reference["url"].as_str() != Some(attachment.url.as_str())
            {
                return Err(invalid());
            }
            let Some(staged) = &attachment.staged else {
                #[cfg(test)]
                if super::mutant("c2_unfetched_ack") {
                    continue;
                }
                return Err(invalid());
            };
            if staged.filename != attachment.filename
                || staged.sha256 != hex::encode(Sha256::digest(&staged.bytes))
            {
                return Err(invalid());
            }
        }
        if attachments.is_empty() {
            return Ok(());
        }
        #[cfg(test)]
        if super::mutant("c2_pin_before_received") {
            return Ok(());
        }
        for (index, attachment) in attachments.iter().enumerate() {
            let Some(staged) = &attachment.staged else {
                continue;
            };
            let index = u32::try_from(index).map_err(|_| invalid())?;
            self.pins.push(ledger.pin_blob(
                &self.key.to_string(),
                index,
                &staged.filename,
                &staged.bytes,
            )?);
        }
        self.input["blob_pins"] = serde_json::to_value(&self.pins)?;
        Ok(())
    }

    pub(crate) fn key(&self) -> u64 {
        self.key
    }
    pub(crate) fn identity(&self) -> &ReceiptIdentity {
        &self.identity
    }
    pub(super) fn input(&self) -> &Value {
        &self.input
    }
    pub(super) fn into_entry(self) -> (Entry, Vec<BlobPin>) {
        (
            Entry::Received {
                key: self.key,
                input: self.input,
            },
            self.pins,
        )
    }
}
