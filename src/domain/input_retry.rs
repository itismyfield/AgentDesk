//! Pure retry classes and the backlog layers a later input would pass.

/// Why an input cannot run or be skipped. Consumers branch on this, never on the detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnavailableReason {
    RootUnavailable,
    ReadFailed,
    MalformedRecord,
    /// Some latch fields present, or one written as null.
    IncompleteRecord,
    InvalidNotice,
    /// The reads do not cover exactly the input's identity.
    IncompleteView,
    /// Preparation is held until the existing observer confirms input readiness.
    IdleDraftRelease,
}

/// How the automatic paths may treat one queued input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryClass {
    /// Runnable; it also holds back inputs that arrived later.
    Ready,
    /// Budget spent: kept, never auto-submitted, and never holds back later inputs.
    Parked,
    /// State unknown: neither run nor skipped, and it holds back later inputs.
    Unavailable(UnavailableReason),
}

/// Whether nothing but parked inputs sits in one queue layer.
pub(crate) fn parked_only(classes: &[RetryClass]) -> bool {
    classes.iter().all(|class| *class == RetryClass::Parked)
}

/// One queue layer as the injection check read it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LayerSnapshot<'a> {
    Classes(&'a [RetryClass]),
    Unreadable,
}

/// The backlog a later input would pass. Both layers are required; one left unchecked refuses.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BacklogLayers<'a> {
    pub memory: Option<LayerSnapshot<'a>>,
    pub disk: Option<LayerSnapshot<'a>>,
}

impl BacklogLayers<'_> {
    /// Only layers read as parked-only admit a later input past them.
    pub fn admits_later_input(&self) -> bool {
        [self.memory, self.disk].iter().all(
            |layer| matches!(layer, Some(LayerSnapshot::Classes(classes)) if parked_only(classes)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::LayerSnapshot::{Classes, Unreadable};
    use super::RetryClass::{Parked, Ready, Unavailable};
    use super::UnavailableReason::ReadFailed;
    use super::*;

    #[test]
    fn backlog_requires_both_layers_to_be_confirmed_parked_only() {
        let cases = [
            ("missing", None, false),
            ("unreadable", Some(Unreadable), false),
            ("empty", Some(Classes(&[])), true),
            ("parked", Some(Classes(&[Parked, Parked])), true),
            ("ready", Some(Classes(&[Ready])), false),
            ("mixed ready", Some(Classes(&[Parked, Ready])), false),
            (
                "unavailable",
                Some(Classes(&[Unavailable(ReadFailed)])),
                false,
            ),
            (
                "mixed unavailable",
                Some(Classes(&[Parked, Unavailable(ReadFailed)])),
                false,
            ),
        ];
        for (memory_name, memory, memory_parked) in cases {
            for (disk_name, disk, disk_parked) in cases {
                assert_eq!(
                    BacklogLayers { memory, disk }.admits_later_input(),
                    memory_parked && disk_parked,
                    "memory={memory_name} disk={disk_name}"
                );
            }
        }
    }
}
