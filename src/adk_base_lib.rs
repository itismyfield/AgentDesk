//! Seed of the shared base crate, built from the files in place.
//! The parent re-exports each module, so `crate::domain::…` and `crate::utils::…` paths keep resolving.
#![deny(clippy::drop_non_drop)]
#![deny(clippy::await_holding_lock)]
#![deny(clippy::mistyped_literal_suffixes)]
#![deny(clippy::correctness)]

pub mod domain {
    pub mod auto_queue_log;
    pub mod automation_contract;
    pub mod bot_role;
    pub mod conversation;
    pub mod dispatch_route;
    pub mod dispatch_summary;
    pub mod fix_safety;
    pub mod host_observation;
    pub mod intake_consent;
    pub mod intake_identity;
    pub mod lifecycle;
    pub mod memory;
    pub mod provider;
    pub mod provider_auth_profile;
    pub mod session_identity;
    pub mod tool_text;
}

pub mod utils {
    pub mod dag;
    pub mod format;
    pub mod github_links;
    pub mod home_path;
    pub mod redact;
}
