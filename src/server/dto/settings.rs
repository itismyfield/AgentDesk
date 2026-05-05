//! Settings route DTO re-exports.
//!
//! The settings service owns the response shapes because it also owns the
//! effective-value/default shaping. Routes import them through this module so
//! handler code stays declarative.

#[allow(unused_imports)]
pub use crate::services::settings::{
    RuntimeConfigResponse, SettingsConfigEntriesResponse, SettingsConfigEntry,
    SettingsConfigPatchResponse, SettingsDocument, SettingsOkResponse,
};

#[derive(Debug, serde::Serialize)]
pub struct SettingsErrorResponse<'a> {
    pub error: &'a str,
}
