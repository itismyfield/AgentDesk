pub(crate) mod args;
pub(crate) mod client;
pub(crate) mod dcserver;
pub(crate) mod direct;
pub(crate) mod discord;
pub(crate) mod doctor;
pub(crate) mod init;
pub(crate) mod migrate;
pub(crate) mod monitoring;
pub(crate) mod provider_cli;
pub(crate) mod run;
pub(crate) mod utils;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// Re-export commonly used items
pub use dcserver::{agentdesk_runtime_root, handle_dcserver, handle_restart_dcserver};
pub use discord::{handle_discord_senddm, handle_discord_sendfile, handle_discord_sendmessage};
pub use init::handle_init;
pub(crate) use run::execute;
