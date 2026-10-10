//! Materialized input with positively named Discord provenance.

use std::io;

use serde_json::Value;

use crate::services::tui_input::blob::BlobPin;
use crate::services::tui_input::rows::Entry;
use crate::services::tui_input::rows::receipt_identity::ReceiptIdentity;

pub(crate) struct Source {
    key: u64,
    identity: ReceiptIdentity,
    input: Value,
    pins: Vec<BlobPin>,
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
        })
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
