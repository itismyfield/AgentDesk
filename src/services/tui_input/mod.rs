//! Pane judgement, bounded tmux effects and prompt submission shared by the live input paths.

pub mod actor;
pub(crate) mod ansi;
pub mod bounded_tmux;
pub mod human_frame;
pub mod input_key;
pub(crate) mod submission;
pub mod submission_tmux;

#[cfg(test)]
mod ansi_tests;
#[cfg(test)]
mod bounded_tmux_tests;
#[cfg(test)]
mod human_frame_tests;
#[cfg(test)]
mod input_key_tests;

pub mod target_epoch;
pub(crate) mod writer_lease;
