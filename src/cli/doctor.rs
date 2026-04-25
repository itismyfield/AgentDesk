//! `agentdesk doctor` — environment diagnostics.

pub(crate) mod contract;
mod health;
mod mailbox;
mod orchestrator;
pub(crate) mod startup;

pub(crate) use orchestrator::{DoctorOptions, cmd_doctor, run_doctor_report};
