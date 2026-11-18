//! Legacy's Claude relay state in this process as the O writer's adoption reads it: cursors, the
//! delivered frontier and running response tails. Nothing here decides or writes.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::super::SharedData;
use super::delivery_record as dr;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::writer::adoption::{LegacyCursor, LegacyEpoch, LegacyView};

pub(in crate::services::discord) struct LegacyRelay {
    shared: Arc<SharedData>,
}

impl LegacyRelay {
    pub(in crate::services::discord) fn new(shared: Arc<SharedData>) -> Self {
        Self { shared }
    }
}

fn late_source_cursor(tmux: &str) -> Result<LegacyCursor, String> {
    let read = |authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>| {
        let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session_under_source_authority(authority);
        match binding.filter(|binding| binding.runtime_kind == RuntimeHandoffKind::ClaudeTui) {
            Some(binding) => Ok(LegacyCursor::Bound {
                path: binding.output_path.into(),
                offset: binding.last_offset,
            }),
            None => match crate::services::tmux_diagnostics::tmux_session_pane_liveness(tmux) {
                crate::services::platform::tmux::PaneLiveness::Live => Ok(LegacyCursor::Unbound),
                crate::services::platform::tmux::PaneLiveness::DeadOrAbsent => {
                    Ok(LegacyCursor::NoPane)
                }
                crate::services::platform::tmux::PaneLiveness::ProbeError => {
                    Err("pane liveness could not be established".into())
                }
            },
        }
    };
    let observed = crate::services::tmux_common::try_with_tmux_source_authority(tmux, read);
    observed.unwrap_or_else(|| Err("source authority is busy".into()))
}

impl LegacyView for LegacyRelay {
    fn started(&self) -> bool {
        crate::services::claude_tui::hook_server::boot_discovery_done()
    }

    fn cursor(&self, tmux: &str) -> LegacyCursor {
        let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux);
        match binding.filter(|binding| binding.runtime_kind == RuntimeHandoffKind::ClaudeTui) {
            Some(binding) => LegacyCursor::Bound {
                path: binding.output_path.into(),
                offset: binding.last_offset,
            },
            None if crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux) => {
                LegacyCursor::Unbound
            }
            None => LegacyCursor::NoPane,
        }
    }

    fn late_cursor(&self, tmux: &str) -> Result<LegacyCursor, String> {
        late_source_cursor(tmux)
    }

    /// The floor the idle response tail takes, over the same arguments.
    fn frontier(&self, channel: u64, tmux: &str, eof: u64) -> Option<u64> {
        let channel = ChannelId::new(channel);
        let claude = &ProviderKind::Claude;
        Some(dr::effective_committed_offset(
            &self.shared,
            claude,
            channel,
            tmux,
            Some(eof),
        ))
    }

    fn historical_frontier(
        &self,
        binding: &crate::services::tui_o::writer::binding::BindingEvent,
        turn: Option<&crate::services::tui_o::writer::adoption::LegacyTurnIdentity>,
        delivery: Option<&crate::services::tui_o::writer::adoption::LegacyDelivery>,
        eof: u64,
    ) -> Option<u64> {
        let scope = delivery?;
        let provider = match binding.provider {
            crate::services::tui_o::shadow::ShadowProvider::Claude => ProviderKind::Claude,
            crate::services::tui_o::shadow::ShadowProvider::Codex => ProviderKind::Codex,
        };
        historical_frontier_in(
            dr::read_record(&provider, scope.owner)?,
            binding,
            turn,
            delivery,
            eof,
        )
    }

    fn tail_running(&self, tmux: &str) -> bool {
        super::super::tui_prompt_relay::claude_idle_tail_running(tmux)
    }

    /// What `relay_auto_heal` restarts a redrive episode on, read without writing the inflight row.
    fn epoch(&self, channel: u64) -> LegacyEpoch {
        let row =
            super::super::inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel);
        self.epoch_from_row(channel, row)
    }
    fn session_ended(&self, tmux: &str) -> Result<bool, String> {
        use crate::services::platform::tmux::PaneLiveness;
        if !super::super::host_liveness::local_tmux(tmux, None) {
            return Ok(false);
        }
        match crate::services::tmux_diagnostics::tmux_session_pane_liveness(tmux) {
            PaneLiveness::Live => Ok(false),
            PaneLiveness::DeadOrAbsent => Ok(true),
            PaneLiveness::ProbeError => Err("session end could not be observed".into()),
        }
    }
    fn late_epoch(&self, channel: u64) -> Result<LegacyEpoch, String> {
        let row = super::super::inflight::load_inflight_state_read_only_result(
            &ProviderKind::Claude,
            channel,
        )?;
        Ok(self.epoch_from_row(channel, row))
    }
}

#[cfg(all(test, unix))]
#[path = "o_writer_late_tmux_tests.rs"]
mod late_tmux;

#[cfg(test)]
mod late_lookup_tests {
    #[test]
    fn late_pin_source_contention_retries_in_isolated_process() {
        const CHILD: &str = "ADK_LATE_PIN_SOURCE_CONTENTION_CHILD";
        if std::env::var_os(CHILD).is_some() {
            crate::services::tmux_common::with_tmux_source_authority(
                "late-pin-source-contention",
                |_| {
                    assert!(super::late_source_cursor("late-pin-source-contention").is_err());
                },
            );
            println!("ADK_LATE_PIN_SOURCE_RETRY_CHECKED");
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "late_pin_source_contention_retries_in_isolated_process",
                "--nocapture",
            ])
            .stdout(std::process::Stdio::piped())
            .env(CHILD, "1")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "isolated source-contention oracle failed");
                let output = child.wait_with_output().unwrap();
                assert!(
                    String::from_utf8_lossy(&output.stdout)
                        .contains("ADK_LATE_PIN_SOURCE_RETRY_CHECKED"),
                    "isolated child must execute the oracle"
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("late source reader blocked while SourceAuthority was held");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

impl LegacyRelay {
    fn epoch_from_row(
        &self,
        channel: u64,
        row: Option<super::super::inflight::InflightTurnState>,
    ) -> LegacyEpoch {
        let id = ChannelId::new(channel);
        let delivery = row.as_ref().and_then(|row| {
            let tmux = row.tmux_session_name.as_deref()?;
            let source_path = row.output_path.as_ref()?.into();
            let generation = row
                .tui_terminal_generation_mtime_ns
                .unwrap_or_else(|| dr::current_generation_mtime_ns(tmux));
            (generation != 0).then(
                || crate::services::tui_o::writer::adoption::LegacyDelivery {
                    source_path,
                    execution_nonce: row.hosted_execution_nonce.clone(),
                    owner: row.delivery_record_owner_channel_id(),
                    generation,
                },
            )
        });
        LegacyEpoch {
            delivery,
            reset_incarnation: self.shared.relay_frontier_token(id).reset_incarnation,
            reconnects: (self.shared.tmux_relay_coord(id).reconnect_count)
                .load(std::sync::atomic::Ordering::Acquire),
            turn: row.map(|row| {
                let nonce = row.turn_nonce.filter(|nonce| !nonce.is_empty());
                let start = row.turn_start_offset;
                (
                    row.user_msg_id,
                    row.started_at,
                    row.tmux_session_name,
                    start,
                    nonce,
                )
            }),
        }
    }
}

fn historical_frontier_in(
    record: dr::DeliveryRecord,
    binding: &crate::services::tui_o::writer::binding::BindingEvent,
    turn: Option<&crate::services::tui_o::writer::adoption::LegacyTurnIdentity>,
    delivery: Option<&crate::services::tui_o::writer::adoption::LegacyDelivery>,
    eof: u64,
) -> Option<u64> {
    let (scope, turn) = (delivery?, turn?);
    let source = crate::services::tui_o::writer::rotation::bound_source(binding)?;
    if scope.source_path != source.path
        || scope
            .execution_nonce
            .as_ref()
            .is_some_and(|nonce| nonce != &binding.execution_nonce)
    {
        return None;
    }
    let (start, nonce) = (turn.3?, turn.4.as_deref().filter(|n| !n.is_empty())?);
    if turn.2.as_deref() != Some(binding.tmux_session.as_str())
        || scope.owner == 0
        || scope.generation == 0
    {
        return None;
    }
    let provider = match binding.provider {
        crate::services::tui_o::shadow::ShadowProvider::Claude => ProviderKind::Claude,
        crate::services::tui_o::shadow::ShadowProvider::Codex => ProviderKind::Codex,
    };
    record
        .confirmed_deliveries
        .iter()
        .filter(|receipt| {
            let source = &receipt.source;
            receipt.message_id != 0
                && receipt.delivery_channel_id == binding.channel_id
                && source.is_authoritative()
                && source.provider == provider.as_str()
                && source.tmux_session_name == binding.tmux_session
                && source.turn_nonce == nonce
                && source.generation_mtime_ns == scope.generation
                && source.offset_authority_channel_id == scope.owner
                && source.delivery_channel_id == binding.channel_id
                && source.range.0 == start
                && source.range.1 <= eof
        })
        .map(|receipt| receipt.source.range.1)
        .max()
}

#[cfg(test)]
mod child_debt_tests;
