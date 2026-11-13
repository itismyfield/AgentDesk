#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentHandoffChannelKind {
    Cc,
    Cdx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("channel_kind must be cc or cdx")]
pub(crate) struct InvalidChannelKind;

impl AgentHandoffChannelKind {
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, InvalidChannelKind> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("cc") => Ok(Self::Cc),
            Some("cdx") => Ok(Self::Cdx),
            Some(_) => Err(InvalidChannelKind),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Cc => "cc",
            Self::Cdx => "cdx",
        }
    }
}
