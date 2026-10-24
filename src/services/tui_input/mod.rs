//! Pane judgement, bounded tmux effects and prompt submission shared by the live input paths.

pub mod actor;
pub mod bounded_tmux;
pub(crate) mod human_frame;
pub mod input_key;
pub(crate) mod submission;
pub(crate) mod submission_tmux;

#[cfg(test)]
mod bounded_tmux_tests;
#[cfg(test)]
mod human_frame_tests;
#[cfg(test)]
mod input_key_tests;

/// A named fault a test run switches on to check that the live input tests catch it.
#[cfg(test)]
pub(crate) fn mutant(name: &str) -> bool {
    std::env::var("ADK_TEST_INPUT_MUTANT").is_ok_and(|value| value == name)
}

pub(crate) mod target_epoch;
