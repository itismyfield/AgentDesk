use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default)]
pub struct DispatchCreateOptions {
    pub skip_outbox: bool,
    pub sidecar_dispatch: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    #[default]
    Text,
    Voice,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Voice => "voice",
        }
    }

    pub fn from_label(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "text" => Some(Self::Text),
            "voice" => Some(Self::Voice),
            _ => None,
        }
    }
}
