//! One channel effort choice produces both the CLI level and the session ultracode flag.
#[derive(Clone, Copy, Default)]
pub(super) enum LaunchEffort<'a> {
    #[default]
    Inherit,
    Level(&'a str),
    Ultra,
}

impl<'a> LaunchEffort<'a> {
    pub(super) fn parse(value: Option<&'a str>) -> Result<Self, String> {
        match value {
            None => Ok(Self::Inherit),
            Some("ultra") => Ok(Self::Ultra),
            Some(level @ ("low" | "medium" | "high" | "xhigh" | "max")) => Ok(Self::Level(level)),
            Some(other) => Err(format!("unsupported Claude channel effort: {other}")),
        }
    }

    pub(super) fn ultracode(self) -> Option<bool> {
        match self {
            Self::Inherit => None,
            Self::Level(_) => Some(false),
            Self::Ultra => Some(true),
        }
    }

    pub(super) fn append_args(self, args: &mut Vec<String>) {
        if let Self::Level(level) = self {
            args.extend(["--effort".into(), level.into()]);
        }
    }

    pub(super) fn explicit(self) -> bool {
        !matches!(self, Self::Inherit)
    }
}
