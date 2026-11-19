//! Types and codecs for injecting a queued message into a live provider turn.
//! Nothing constructs or consumes them yet, so they carry no runtime policy.
#![allow(dead_code)]

pub(crate) mod action_handle;
pub(crate) mod admission_intent;
mod fixed_str;
mod handle_value;
pub(crate) mod operation;
pub(crate) mod record;
