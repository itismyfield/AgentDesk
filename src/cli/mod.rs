pub(crate) mod dcserver;
pub(crate) mod discord;
pub(crate) mod utils;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// Re-export commonly used items
pub use dcserver::{
    handle_dcserver, handle_restart_dcserver, parse_restart_dcserver_report_context,
    remotecc_runtime_root,
};
pub use discord::{handle_discord_sendfile, handle_discord_sendmessage, handle_discord_senddm};
