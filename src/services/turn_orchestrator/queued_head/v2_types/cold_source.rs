//! Bounded source evidence kept independently of a locally submitted queue head.
use crate::domain::human_input::native_source::{Source, Ticket};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PreKeyPin {
    pub(crate) binding_seq: u64,
    pub(crate) session_id: String,
    pub(crate) transcript: PathBuf,
    pub(crate) offset: u64,
    /// None proves NotFound before the first key; an unreadable file never creates a pin.
    pub(crate) file: Option<[u64; 2]>,
    pub(crate) owner_root: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BindingIntent {
    pub(crate) ticket: Ticket,
    pub(crate) pin: PreKeyPin,
    pub(crate) selected: Option<Source>,
}
