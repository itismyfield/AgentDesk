/// What an admission may do to the composer. There is deliberately no default:
/// order, owner kind or Immediate never imply Restore.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdmissionIntent {
    /// Creates new composer input, Immediate included; deferred while a manual reservation holds.
    NewInput,
    /// Re-registers an existing episode or runtime and grants no new composer input.
    Restore,
}
