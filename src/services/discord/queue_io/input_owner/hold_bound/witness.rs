//! Positive idle evidence from a readable pane and its own transcript.
use std::path::Path;

use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude::herdr_turn::composer;
use crate::services::claude_tui::{
    activity::processes::{self, Descendants},
    busy_inject::{self as claude_screen_read, HeldRead, Pane},
    input::{final_prompt_ready, prompt_readiness_snapshot_from_capture},
    readiness as claude_readiness,
};
use crate::services::codex_tui::{
    busy_inject::screen::{self, Composer},
    input::strip_ansi_escape_sequences,
    readiness as codex_readiness,
};
use crate::services::discord::{
    SharedData, host_liveness,
    inflight::InflightTurnState,
    turn_lease::{LeaseIdentity, matching_inflight_for_hold_bound},
};
use crate::services::platform::tmux::PaneLiveness;
use crate::services::provider::ProviderKind;
use crate::services::session_host::{HerdrTarget, TmuxHost, herdr_endpoints};
use crate::services::tmux_common;
use crate::services::tui_prompt_dedupe::{TuiRuntimeBinding, runtime_binding_for_tmux_session};
use crate::services::tui_turn_state::{self, ToolCalls, TuiTurnState};
use crate::services::turn_host::{self, TurnHost};
use serenity::model::id::ChannelId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Observation {
    Idle(&'static str),
    NotIdle(&'static str),
    Unknown(&'static str),
}

use Observation::{Idle, NotIdle, Unknown};

#[derive(Clone, Copy)]
enum Host {
    Herdr,
    Tmux,
}

impl Host {
    fn label(self) -> &'static str {
        match self {
            Self::Herdr => "herdr",
            Self::Tmux => "tmux",
        }
    }
}

#[derive(Clone, Copy)]
enum End {
    Proven,
    Open,
    Unknown,
}

fn panes_and_transcript(
    host: Host,
    provider: &ProviderKind,
    capture: Option<&str>,
    end: impl FnOnce() -> End,
) -> Observation {
    let Some(capture) = capture.filter(|capture| !capture.trim().is_empty()) else {
        return Unknown("capture_failed");
    };
    let veto = match provider {
        ProviderKind::Claude => claude_screen(host, capture),
        ProviderKind::Codex => codex_screen(capture),
        _ => return Unknown("provider_unsupported"),
    };
    if let Some(veto) = veto {
        return veto;
    }
    match end() {
        End::Proven => Idle(host.label()),
        End::Open => NotIdle("running"),
        End::Unknown => Unknown("transcript_unread"),
    }
}

fn codex_screen(capture: &str) -> Option<Observation> {
    let shown = screen::read(capture);
    if screen::delivery_veto(capture).is_some() || shown.modal {
        return Some(NotIdle("modal"));
    }
    if shown.queued {
        return Some(NotIdle("running"));
    }
    if shown.attachments {
        return Some(NotIdle("draft"));
    }
    match shown.composer {
        Composer::Empty => None,
        Composer::Text(_) => Some(NotIdle("draft")),
        Composer::Unread => Some(Unknown("composer_unread")),
    }
}

fn claude_screen(host: Host, capture: &str) -> Option<Observation> {
    let plain = strip_ansi_escape_sequences(capture);
    if claude_screen_read::modal(capture) || composer::dialog_open(capture) {
        return Some(NotIdle("modal"));
    }
    if tmux_common::tmux_capture_indicates_claude_tui_busy(&plain) {
        return Some(NotIdle("running"));
    }
    if tmux_common::tmux_capture_indicates_claude_tui_prompt_draft(&plain) {
        return Some(NotIdle("draft"));
    }
    match claude_screen_read::held_read(capture) {
        HeldRead::Busy => Some(NotIdle("running")),
        HeldRead::Text(_) => Some(NotIdle("draft")),
        HeldRead::Unread => Some(Unknown("composer_unread")),
        HeldRead::Empty => {
            let ready = match host {
                Host::Herdr => composer::restored_screen_idle(capture),
                Host::Tmux => {
                    let snapshot = prompt_readiness_snapshot_from_capture(Some(&plain), true);
                    final_prompt_ready(&snapshot)
                }
            };
            (!ready).then_some(Unknown("prompt_unready"))
        }
    }
}

pub(super) async fn observe(
    shared: &SharedData,
    channel: u64,
    expected: &LeaseIdentity,
) -> Observation {
    let provider = match expected.provider.as_str() {
        "claude" => &ProviderKind::Claude,
        "codex" => &ProviderKind::Codex,
        _ => return Unknown("provider_unsupported"),
    };
    #[cfg(test)]
    if let Some((sample, claude_source)) = super::super::hold_bound_tests::observation(channel) {
        use super::super::hold_bound_tests::Transcript;
        let host = if sample.host == "herdr" {
            Host::Herdr
        } else {
            Host::Tmux
        };
        return panes_and_transcript(host, provider, sample.capture, || {
            if let Some(source) = claude_source {
                return claude_end(&source.session, &source.path);
            }
            match sample.transcript {
                Transcript::Ended => End::Proven,
                Transcript::Open => End::Open,
                Transcript::Unknown => End::Unknown,
            }
        });
    }

    let Ok(row) = matching_inflight_for_hold_bound(provider, expected) else {
        return Unknown("inflight_unread");
    };
    let session = match row.as_ref().and_then(|row| row.tmux_session_name.clone()) {
        Some(session) => Some(session),
        None => match shared.mailbox_peek(ChannelId::new(channel)) {
            Some(mailbox) => {
                let snapshot = mailbox.snapshot().await;
                snapshot
                    .cancel_token
                    .as_ref()
                    .filter(|_| {
                        snapshot.active_turn_nonce.as_deref() == Some(expected.turn_nonce.as_str())
                    })
                    .and_then(|token| token.tmux_session_name())
            }
            None => None,
        },
    };
    let Some(session) = session.filter(|session| !session.trim().is_empty()) else {
        return Unknown("binding_unavailable");
    };
    let host = turn_host::for_observation(
        shared.pg_pool.as_ref(),
        provider,
        channel,
        row.as_ref().and_then(|row| row.session_key.as_deref()),
    )
    .await;
    let provider = provider.clone();
    let token_hash = shared.token_hash.clone();
    tokio::task::spawn_blocking(move || match host {
        TurnHost::Tmux if host_liveness::local_tmux(&session, row.as_ref()) => {
            read_tmux(&provider, &session, row.as_ref())
        }
        TurnHost::Herdr(plan) => {
            let Some(observed) = plan.row.as_ref() else {
                return Unknown("binding_unavailable");
            };
            let HostedRecord::Known(record) = &observed.record else {
                return Unknown("binding_unavailable");
            };
            if record.state != HostedState::Bound
                || record.owner.logical_key != session
                || record.owner.provider != provider.as_str()
                || record.owner.channel_id != channel.to_string()
                || record.owner.discord_token_hash != token_hash
                || record.owner.owner_node != plan.endpoint.execution_node
                || crate::config::runtime_root().as_deref()
                    != Some(Path::new(&record.owner.runtime_root))
                || row.as_ref().is_some_and(|row| {
                    row.hosted_record_id
                        .is_some_and(|id| id != observed.session_id())
                        || row
                            .hosted_execution_nonce
                            .as_deref()
                            .is_some_and(|nonce| nonce != record.execution_nonce)
                })
            {
                return Unknown("binding_changed");
            }
            let Some(target) = herdr_endpoints().target_with_evidence(record, Default::default())
            else {
                return Unknown("host_unavailable");
            };
            read_herdr(&provider, &session, row.as_ref(), &target)
        }
        _ => Unknown("host_unavailable"),
    })
    .await
    .unwrap_or(Unknown("observation_failed"))
}

fn source(
    provider: &ProviderKind,
    session: &str,
    row: Option<&InflightTurnState>,
) -> Option<TuiRuntimeBinding> {
    let kind = match provider {
        ProviderKind::Claude => RuntimeHandoffKind::ClaudeTui,
        ProviderKind::Codex => RuntimeHandoffKind::CodexTui,
        _ => return None,
    };
    let binding = runtime_binding_for_tmux_session(session)?;
    let matches_row = row.is_none_or(|row| {
        !row.runtime_kind_unknown_on_disk
            && row
                .runtime_kind
                .is_none_or(|kind| kind == binding.runtime_kind)
            && row
                .session_id
                .as_ref()
                .is_none_or(|native| binding.session_id.as_ref() == Some(native))
            && row.output_path.as_deref().is_none_or(|path| {
                path.is_empty()
                    || path == binding.output_path
                    || path == binding.relay_output_path()
            })
    });
    (matches_row && binding.runtime_kind == kind && !binding.output_path.trim().is_empty())
        .then_some(binding)
}

fn binding_unchanged(
    session: &str,
    binding: &TuiRuntimeBinding,
    observed: Observation,
) -> Observation {
    if matches!(observed, Idle(_))
        && !runtime_binding_for_tmux_session(session).is_some_and(|current| {
            current.runtime_kind == binding.runtime_kind
                && current.session_id == binding.session_id
                && current.output_path == binding.output_path
        })
    {
        Unknown("binding_changed")
    } else {
        observed
    }
}

fn read_tmux(
    provider: &ProviderKind,
    session: &str,
    row: Option<&InflightTurnState>,
) -> Observation {
    if TmuxHost.pane_liveness(session) != PaneLiveness::Live {
        return Unknown("pane_unavailable");
    }
    let Some(binding) = source(provider, session, row) else {
        return Unknown("binding_unavailable");
    };
    let Ok(pane) = Pane::with_program(session, "tmux".into()) else {
        return Unknown("pane_unavailable");
    };
    let capture = pane.capture_visible();
    let observed = panes_and_transcript(Host::Tmux, provider, capture.as_deref(), || {
        match processes::observe(session) {
            Descendants::Present => return End::Open,
            Descendants::Unknown => return End::Unknown,
            Descendants::NoneObserved => (),
        }
        match provider {
            ProviderKind::Claude => claude_end(session, Path::new(&binding.output_path)),
            ProviderKind::Codex => codex_end(codex_readiness::judge_in(session, None)),
            _ => End::Unknown,
        }
    });
    binding_unchanged(session, &binding, observed)
}

fn read_herdr(
    provider: &ProviderKind,
    session: &str,
    row: Option<&InflightTurnState>,
    pane: &HerdrTarget,
) -> Observation {
    let Some(binding) = source(provider, session, row) else {
        return Unknown("binding_unavailable");
    };
    let capture = match pane.hold_bound_capture() {
        Ok(Some(capture)) => capture,
        Ok(None) => return NotIdle("running"),
        Err(()) => return Unknown("capture_failed"),
    };
    let observed = panes_and_transcript(Host::Herdr, provider, Some(&capture), || match provider {
        ProviderKind::Claude => claude_end(session, Path::new(&binding.output_path)),
        ProviderKind::Codex => {
            let nonce = pane.execution_nonce();
            let Ok(context) =
                crate::services::tui_prompt_dedupe::binding_context::execution_context(
                    "codex", nonce,
                )
            else {
                return End::Unknown;
            };
            let pane = codex_readiness::Pane {
                liveness: PaneLiveness::Live,
                nonce: Some(nonce.to_owned()),
                floor: Some(context.created_at),
                source: binding.output_path.clone().into(),
                children: &|| Descendants::Unknown,
            };
            codex_end(codex_readiness::decide(pane, chrono::Utc::now()))
        }
        _ => End::Unknown,
    });
    binding_unchanged(session, &binding, observed)
}

fn claude_end(session: &str, path: &Path) -> End {
    let state = tui_turn_state::observe_claude_jsonl_running_turn_state(path);
    if state.is_busy() {
        return End::Open;
    }
    match tui_turn_state::provider_tool_calls(&ProviderKind::Claude, path) {
        ToolCalls::Open => End::Open,
        ToolCalls::Inconclusive => End::Unknown,
        ToolCalls::Closed
            if state == TuiTurnState::Idle
                && (tui_turn_state::jsonl_completion_scan_idle(&ProviderKind::Claude, path)
                    || claude_readiness::observe(session, Some(path)).is_some()) =>
        {
            End::Proven
        }
        ToolCalls::Closed if claude_readiness::observe_terminal_completion(session, path) => {
            End::Proven
        }
        ToolCalls::Closed => End::Unknown,
    }
}

fn codex_end(verdict: codex_readiness::Verdict) -> End {
    match verdict {
        codex_readiness::Verdict::Idle(_) => End::Proven,
        codex_readiness::Verdict::Busy(_) => End::Open,
        codex_readiness::Verdict::Unknown(_) | codex_readiness::Verdict::Gone => End::Unknown,
    }
}
