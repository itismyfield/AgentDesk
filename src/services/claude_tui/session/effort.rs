//! One channel effort choice produces both the CLI level and the session ultracode flag.
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// How long a restart's mark covers the resume launches of its session.
const OWNER_FIRST_FOR: Duration = Duration::from_secs(600);
static OWNER_FIRST: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Mutex::default);

/// An effort restart cut `session`'s turn and owns the first submit of its next resume.
pub(crate) fn expect_owner_first_submit(session: &str) {
    let mut marks = OWNER_FIRST.lock().unwrap_or_else(|e| e.into_inner());
    marks.retain(|_, at| at.elapsed() < OWNER_FIRST_FOR);
    marks.insert(session.to_string(), Instant::now());
}

/// Whether a resume of `session` launches under a restart whose continuation submits first.
pub(crate) fn owner_first_submit(session: &str) -> bool {
    let marks = OWNER_FIRST.lock().unwrap_or_else(|e| e.into_inner());
    marks
        .get(session)
        .is_some_and(|at| at.elapsed() < OWNER_FIRST_FOR)
}

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
