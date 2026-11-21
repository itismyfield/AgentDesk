pub mod api;
pub mod async_bridge;
pub mod auth;
pub(crate) mod bridge_deadline;
pub use adk_base::utils::dag;
pub mod discord;
mod env_flag;
pub use env_flag::env_flag;

#[cfg(test)]
pub(crate) fn parse_env_flag_for_test(name: &str, raw: Option<&str>, default: bool) -> bool {
    raw.map_or(default, |raw| env_flag::parse_value(name, raw, default))
}

pub use adk_base::utils::format;
pub use adk_base::utils::github_links;
pub use adk_base::utils::home_path;
pub(crate) mod input_validation;
pub mod loopback_url;
pub(crate) mod parsed_bytes;
pub use adk_base::utils::redact;
pub mod secret_file;
// #1099/#3792 WIP detector used by the Discord turn-end warning facade.
pub mod wip_detect;
