//! KRX (tui_o) tests that call discord-layer items, declared here so they leave the KRX crate.
//! Mounted ungated, so each `mod` keeps its original declaration's exact cfg; files stay put.

#[cfg(all(test, unix))]
#[path = "../tui_o/writer/input_notices/admission_tests.rs"]
pub(crate) mod admission;
#[cfg(test)]
#[path = "../tui_o/exact_pg_tests.rs"]
mod exact_pg;
#[cfg(test)]
#[cfg(unix)]
#[path = "../tui_o/writer/executed_resume_tests.rs"]
mod executed_resume;
#[cfg(test)]
#[cfg(unix)]
#[path = "../tui_o/writer/input_notice_transport_tests.rs"]
pub(crate) mod input_notice_transport;
#[cfg(test)]
#[path = "../tui_o/store/ledger/input_notices/tests.rs"]
mod ledger_input_notices;
#[cfg(all(test, unix))]
#[path = "../tui_o/writer/actor/notice_work_tests.rs"]
mod notice_work;
#[cfg(test)]
#[path = "outbound/o_writer_repost_io_tests.rs"]
pub(crate) mod repost_io;
#[cfg(test)]
#[path = "../tui_o/repost/probe_tests.rs"]
pub(crate) mod repost_probe;
