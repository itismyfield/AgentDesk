//! Service facade for the auto-queue writes whose run completion notice is staged in the same tx.
//! Entry points reach the existing DB transaction bodies only through these names.

mod completion;
mod consultation;
mod dispatch_failure;
mod entries;
mod phase_gates;
mod retry;

pub(crate) use completion::*;
pub(crate) use consultation::*;
pub(crate) use dispatch_failure::*;
pub(crate) use entries::*;
pub(crate) use phase_gates::*;
pub(crate) use retry::*;
