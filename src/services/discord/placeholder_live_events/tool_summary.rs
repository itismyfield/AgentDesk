//! Placeholder-side view of `domain::tool_summary`; its tests keep this module path.
#[cfg(test)]
use crate::domain::tool_summary::{description_text, text};

#[cfg(test)]
#[path = "tool_summary_tests.rs"]
mod tool_summary_tests;

#[cfg(test)]
#[path = "tool_summary_label_tests.rs"]
mod tool_summary_label_tests;

#[cfg(test)]
#[path = "tool_summary_accuracy_tests.rs"]
mod accuracy_tests;
