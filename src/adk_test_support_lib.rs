//! Test helpers built as their own crate from the files in place; only dev-dependencies link it.
//! The parent re-exports the module under its test / `test-support` gate, so `crate::test_support::…` paths keep resolving.
#![deny(clippy::drop_non_drop)]
#![deny(clippy::await_holding_lock)]
#![deny(clippy::mistyped_literal_suffixes)]
#![deny(clippy::correctness)]

pub mod test_support;
