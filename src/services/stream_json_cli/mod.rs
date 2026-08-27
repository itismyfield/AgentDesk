//! Provider-neutral process, codec, session, and tool-policy substrate for line-delimited JSON CLIs.

pub mod codec;
pub mod dialects;
pub mod policy;
pub mod request;
pub mod runner;
pub mod session;

pub use policy::{AgentTool, ConfiguredToolPolicy, ToolPolicy};
pub use request::ProviderTurnRequest;
pub use session::ProviderSessionToken;
