//! Where an input came from and how its target actually executes are separate axes; only the
//! execution decides whether the TUI owner takes the effect. Later slices add variants here.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputOrigin {
    Discord,
    HttpStart,
    Routine,
    DirectHeadless,
}

/// The verified execution of the target, never read from an origin name or a source string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Execution {
    LocalTui,
    /// A TUI on another node: placement hands it to that node's owner.
    RemoteTui,
    Process,
    NonTuiProvider,
}

impl InputOrigin {
    pub(crate) const ALL: [Self; 4] = [
        Self::Discord,
        Self::HttpStart,
        Self::Routine,
        Self::DirectHeadless,
    ];
}

impl Execution {
    pub(crate) const ALL: [Self; 4] = [
        Self::LocalTui,
        Self::RemoteTui,
        Self::Process,
        Self::NonTuiProvider,
    ];

    /// A TUI target goes to the owner whatever the origin; a process or non-TUI provider keeps
    /// its own path.
    pub(crate) fn owner_takes(self, _origin: InputOrigin) -> bool {
        matches!(self, Self::LocalTui | Self::RemoteTui)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_execution_alone_decides_whether_the_owner_takes_an_input() {
        for origin in InputOrigin::ALL {
            let taken: Vec<_> = (Execution::ALL.into_iter())
                .filter(|execution| execution.owner_takes(origin))
                .collect();
            assert_eq!(
                taken,
                [Execution::LocalTui, Execution::RemoteTui],
                "{origin:?}"
            );
        }
    }
}
