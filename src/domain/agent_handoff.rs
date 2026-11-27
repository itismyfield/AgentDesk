#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentHandoffChannelKind {
    Cc,
    Cdx,
}

impl AgentHandoffChannelKind {
    pub fn parse(value: Option<&str>) -> Result<Self, &'static str> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("cc") => Ok(Self::Cc),
            Some("cdx") => Ok(Self::Cdx),
            Some(_) => Err("channel_kind must be cc or cdx"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cc => "cc",
            Self::Cdx => "cdx",
        }
    }
}
