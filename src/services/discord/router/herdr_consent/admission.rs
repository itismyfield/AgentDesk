//! Fresh input waits in its original durable custody before any consent card effect.
use super::super::intake_dispatch::{IntakeOrigin, IntakeSubmission};
use super::super::message_handler::{IntakeDeps, IntakeRequest};
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    self as hosted, HostedLookup, HostedRecord, HostedState,
};
use crate::services::session_host::policy::{LocalChecker, ProofCache};
use consent::{ConsentKey, IntentRef, Preview, Question, workflow::Request};

#[derive(Debug, Clone)]
pub(in crate::services::discord::router) struct Hold {
    key: ConsentKey,
    preview: Preview,
    owner: u64,
    bot: u64,
}

/// None is the unchanged path. The mode guard precedes rows, files, checker and Discord reads.
pub(in crate::services::discord::router) async fn check(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    request: &IntakeRequest,
) -> Option<Result<Hold, String>> {
    let configured = endpoint(request.channel_id.get())?;
    // Existing routing owns remote custody; only its execution-node worker observes policy.
    if session_hosts::local_node().as_deref() != Some(configured.execution_node.as_str()) {
        return None;
    }
    match assess(deps, provider, request, configured).await {
        Ok(None) => None,
        other => Some(other.and_then(|hold| hold.ok_or("consent assessment missing".into()))),
    }
}

async fn assess(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    request: &IntakeRequest,
    configured: HerdrLaunchEndpoint,
) -> Result<Option<Hold>, String> {
    require_local(&configured)?;
    let records = records().await?;
    let mut previous_hold = None;
    for consent in records.iter().filter(|record| {
        record.provider == provider.as_str()
            && record.key.node == configured.execution_node
            && record.key.socket == std::path::Path::new(&configured.socket_addr)
            && record.key.named_session == configured.herdr_session
    }) {
        let sink = custody::LiveSink::new(deps.shared.clone(), provider.clone(), &consent.key)?;
        let mut held = false;
        for intent in consent.unresolved_intents().iter().filter(|intent| {
            intent.channel == request.channel_id.get()
                && (intent.source_message == request.user_msg_id.get()
                    || request
                        .source_message_ids
                        .iter()
                        .any(|id| id.get() == intent.source_message))
        }) {
            held |= !sink.released(intent).await?;
        }
        let current = Hold {
            key: consent.key.clone(),
            preview: consent.preview.clone(),
            owner: consent.owner,
            bot: consent.bot,
        };
        if held {
            return Ok(Some(current));
        }
        if matches!(
            consent.state,
            State::Denied | State::Expired | State::Accepted | State::ApplyFailed
        ) {
            previous_hold = Some(current);
        }
    }
    let pool = deps
        .shared
        .pg_pool
        .as_ref()
        .ok_or("herdr admission row unavailable; preserve input")?;
    let channel = request.channel_id.get().to_string();
    use crate::db::dispatched_session_canonical_identity::{
        CanonicalSessionIdentity, SessionIdentityKind,
    };
    let key = hosted::HostedLookupKey::Canonical {
        provider: provider.as_str(),
        identity: CanonicalSessionIdentity {
            kind: SessionIdentityKind::DiscordChannel,
            discord_token_hash: &deps.shared.token_hash,
            channel_id: &channel,
        },
    };
    match hosted::load_hosted_execution_pg(pool, key).await {
        HostedLookup::Found(row) => match row.record {
            HostedRecord::Known(record)
                if matches!(record.state, HostedState::Pending | HostedState::Bound) =>
            {
                return Ok(None);
            }
            HostedRecord::Unknown(_) => {
                return Err("unknown hosted execution; preserve input".into());
            }
            _ => {}
        },
        HostedLookup::Missing => {}
        other => return Err(format!("herdr admission row unverified: {other:?}")),
    }
    if let Some(held) = previous_hold {
        return Ok(Some(held));
    }
    let source = peer(&configured)?;
    static PROOFS: std::sync::OnceLock<ProofCache> = std::sync::OnceLock::new();
    if PROOFS
        .get_or_init(ProofCache::default)
        .next_restart(source.clone(), &LocalChecker)
        .await
        .is_ok()
    {
        return Ok(None);
    }
    let prepared = preview::prepare(source, 1).await?;
    let key = ConsentKey {
        node: configured.execution_node,
        socket: configured.socket_addr.into(),
        named_session: configured.herdr_session,
        // SAFETY: geteuid has no preconditions and cannot fail.
        user: unsafe { libc::geteuid() },
        config: prepared.logical().into(),
        target: prepared.target().into(),
    }
    .normalized()?;
    let previous = records.into_iter().find(|record| record.key == key);
    let mut token = prepared.token;
    if let Some(previous) = previous {
        token.revision = previous.preview.revision;
        if token != previous.preview {
            token.revision = token
                .revision
                .checked_add(1)
                .ok_or("consent revision exhausted")?;
        }
    }
    let owner = deps
        .shared
        .settings
        .read()
        .await
        .owner_user_id
        .ok_or("configure an owner before herdr consent")?;
    let bot = deps
        .http
        .get_current_user()
        .await
        .map_err(|e| e.to_string())?
        .id
        .get();
    Ok(Some(Hold {
        key,
        preview: token,
        owner,
        bot,
    }))
}

impl Hold {
    fn question(&self, intent: IntentRef) -> Question {
        Question {
            key: self.key.clone(),
            preview: self.preview.clone(),
            owner: self.owner,
            bot: self.bot,
            provider: intent.provider.clone(),
            channel: intent.channel,
            message: 0,
            intent,
        }
    }
    async fn record(&self, intents: Vec<IntentRef>) -> Result<Consent, String> {
        let hold = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut current = None;
            for intent in intents {
                current = Some(
                    store()
                        .hold_unbound(hold.question(intent), now())?
                        .into_consent(),
                );
            }
            current.ok_or("durable custody has no original source references".into())
        })
        .await
        .map_err(|e| e.to_string())?
    }
    async fn effect(
        &self,
        deps: &IntakeDeps<'_>,
        provider: &ProviderKind,
        consent: Consent,
        explicit: bool,
    ) -> Result<(), String> {
        let intent = consent
            .intents
            .last()
            .cloned()
            .ok_or("consent intent missing")?;
        let event = Event::Hold {
            question: Box::new(self.question(intent)),
            now: now(),
            request: if explicit {
                Request::ExplicitLaunch
            } else {
                Request::Automatic
            },
        };
        run(deps.http, deps.shared, provider, consent, event, None).await
    }
}

pub(in crate::services::discord::router) async fn register_mailbox(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    request: &IntakeRequest,
    hold: Hold,
    explicit: bool,
) -> Result<(), String> {
    let (provider_read, token_hash, channel) = (
        provider.clone(),
        deps.shared.token_hash.clone(),
        request.channel_id,
    );
    let (items, _) = tokio::task::spawn_blocking(move || {
        crate::services::turn_orchestrator::load_channel_pending_queue_checked(
            &provider_read,
            &token_hash,
            channel,
        )
    })
    .await
    .map_err(|e| e.to_string())??;
    let mut wanted = request.source_message_ids.clone();
    wanted.push(request.user_msg_id);
    wanted.sort_unstable();
    wanted.dedup();
    let mut intents = Vec::new();
    for source in wanted {
        let found = items
            .iter()
            .flat_map(|item| item.source_message_queued_generations())
            .find(|s| s.message_id == source)
            .ok_or("original source not durably queued; checkpoint remains unread")?;
        intents.push(IntentRef {
            store: "mailbox".into(),
            id: format!("{}:{}", source.get(), found.queued_generation),
            provider: provider.as_str().into(),
            channel: channel.get(),
            source_message: source.get(),
            fifo: found.queued_generation,
        });
    }
    let consent = hold.record(intents).await?;
    // Card failure never relinquishes custody or retries a send whose response was lost.
    if let Err(error) = hold.effect(deps, provider, consent, explicit).await {
        tracing::warn!(%error, "consent card held after durable input");
    }
    Ok(())
}

pub(in crate::services::discord::router) async fn hold_live(
    deps: &IntakeDeps<'_>,
    mut submission: IntakeSubmission,
    hold: Result<Hold, String>,
) -> Result<(), Error> {
    let uploads = super::super::intake_dispatch::turn_mode_uploads(
        deps,
        &submission,
        &submission.request.channel_id.get().to_string(),
    )
    .await?;
    submission.preloaded_uploads.extend(uploads);
    submission.attachments.clear();
    let (provider, request) = (submission.provider.clone(), submission.request.clone());
    let explicit = !matches!(submission.origin, IntakeOrigin::QueuedDrain(_));
    if !super::super::intake_dispatch::defer_live_submission(deps, submission, true).await {
        return Err("herdr consent durable custody unavailable; checkpoint remains unread".into());
    }
    register_mailbox(deps, &provider, &request, hold?, explicit)
        .await
        .map_err(Into::into)
}

pub(in crate::services::discord::router) async fn worker(
    deps: &IntakeDeps<'_>,
    request: &IntakeRequest,
    proof: Option<crate::services::cluster::intake_worker::BeforeEffects>,
) -> bool {
    let provider = &deps.shared.provider;
    let Some(held) = check(deps, provider, request).await else {
        return false;
    };
    let result = async {
        let hold = held?;
        let proof = proof.ok_or("consent requires an exact before-effects worker handoff")?;
        let row = proof.row();
        let intent = IntentRef {
            store: "intake_outbox".into(),
            id: row.id.to_string(),
            provider: row.provider.clone(),
            channel: request.channel_id.get(),
            source_message: request.user_msg_id.get(),
            fifo: row.attempt_no as u64,
        };
        let consent = hold.record(vec![intent.clone()]).await?;
        let pool = deps
            .shared
            .pg_pool
            .as_ref()
            .ok_or("worker custody database unavailable")?;
        crate::db::intake_outbox::consent_handoff::hold_before_effects(
            pool,
            proof,
            &store(),
            &consent,
            &intent,
        )
        .await?;
        hold.effect(deps, provider, consent, false).await
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, "worker consent keeps original payload deferred");
    }
    true
}

#[derive(Debug)]
pub(in crate::services::discord::router) struct QueuedHold(pub Result<Hold, String>);
impl std::fmt::Display for QueuedHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("consent awaits original head restoration")
    }
}
impl std::error::Error for QueuedHold {}

#[cfg(test)]
#[path = "admission_tests.rs"]
pub(in crate::services::discord::router) mod tests;
