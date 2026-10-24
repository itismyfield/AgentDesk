//! Test-only cold effect boundary; the owner retains settlement and observer custody.
use crate::services::turn_orchestrator::ChannelMailboxHandle;

pub(super) struct Completion<K, O> {
    pub attempt: K,
    pub outcome: Result<O, String>,
}

pub(super) async fn submit<O: Send + 'static>(
    mailbox: &ChannelMailboxHandle,
    now: u64,
    effect: impl FnOnce(String) -> Result<O, String> + Send + 'static,
) -> Result<Completion<impl serde::Serialize + Send, O>, String> {
    let (attempt, permit, frame) = mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("V2 not adopted")?;
            let key = owner.active_attempt()?;
            if !matches!(key.attempt.provider.as_str(), "claude" | "codex")
                || !matches!(key.attempt.target.host.as_str(), "tmux" | "herdr")
            {
                return Err("provider_unsupported".into());
            }
            let frame = owner
                .snapshot
                .head_injection
                .as_ref()
                .and_then(|head| head.payload.as_ref())
                .ok_or("unprepared payload")?
                .frame
                .clone();
            let history =
                crate::services::tui_input::human_frame::history::v2::Registry::runtime()?;
            owner.prepare_history(&history)?;
            let permit = owner.arm(&key, now)?;
            Ok((key, permit, frame))
        })
        .await?;
    let fallback = attempt.clone();
    Ok(tokio::task::spawn_blocking(move || {
        let outcome = permit.consume(&attempt).and_then(|_writer| effect(frame));
        Completion { attempt, outcome }
    })
    .await
    .unwrap_or_else(|error| Completion {
        attempt: fallback,
        outcome: Err(error.to_string()),
    }))
}

#[cfg(test)]
#[path = "first_input/pg_tests.rs"]
mod pg_tests;
