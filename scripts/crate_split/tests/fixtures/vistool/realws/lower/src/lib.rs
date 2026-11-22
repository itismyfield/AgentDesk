//! vistool real-mode fixture: the lower member as a cut leaves it, before its items are widened.
pub(crate) mod core;

pub fn version() -> u32 {
    core::helper()
}
