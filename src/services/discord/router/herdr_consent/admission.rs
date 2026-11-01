//! Fresh input waits in its original durable custody before any consent card effect.
use super::super::intake_dispatch::{IntakeOrigin, IntakeSubmission};
use super::super::message_handler::{IntakeDeps, IntakeRequest};
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    self as hosted, HostedLookup, HostedRecord, HostedState,
};
use crate::services::session_host::{
    herdr_endpoints,
    policy::{LocalChecker, ProofCache, Unverified},
};
use consent::{ConsentKey, IntentRef, Preview, Question, workflow::Request};
use refusal::Code;
pub(in crate::services::discord) use refusal::Refusal;

#[path = "admission_refusal.rs"]
mod refusal;

#[derive(Debug, Clone)]
pub(in crate::services::discord) struct Hold {
    key: ConsentKey,
    preview: Preview,
    owner: u64,
    bot: u64,
    unreleased: Vec<IntentRef>,
}

/// None is the unchanged path. The mode guard precedes rows, files, checker and Discord reads.
pub(in crate::services::discord::router) async fn check(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    request: &IntakeRequest,
) -> Option<Result<Hold, Refusal>> {
    let configured = endpoint(request.channel_id.get())?;
    // Existing routing owns remote custody; only its execution-node worker observes policy.
    if session_hosts::local_node().as_deref() != Some(configured.execution_node.as_str()) {
        return None;
    }
    // An input of this local user_default channel is demand for its server; rows decide later.
    Box::pin(crate::services::session_host::autostart::spawn::ensure(
        &configured,
    ))
    .await;
    match Box::pin(assess(deps, provider, request, configured)).await {
        Ok(None) => None,
        other => Some(other.and_then(|hold| {
            hold.ok_or_else(|| Code::Missing.of()("consent assessment missing".into()))
        })),
    }
}

/// The launch host's policy cache, so a passing check also stands for the host's later permits
/// and mutations; a registry without a launch host keeps a cache of its own.
async fn observe(
    configured: &HerdrLaunchEndpoint,
    source: Arc<dyn PeerSource>,
) -> (
    Result<crate::services::session_host::policy::NextRestartProof, Unverified>,
    serde_json::Value,
) {
    let registry = herdr_endpoints();
    if let Some(host) = registry.launch_host() {
        return host.observe_policy_from(configured, source).await;
    }
    static PROOFS: std::sync::OnceLock<ProofCache> = std::sync::OnceLock::new();
    let proofs = PROOFS.get_or_init(ProofCache::default);
    let checked = proofs.next_restart(source, &LocalChecker).await;
    let socket = std::path::Path::new(&configured.socket_addr);
    let observation =
        crate::services::session_host::policy::diagnostics::record(socket, proofs, &checked);
    (checked, observation)
}

async fn assess(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    request: &IntakeRequest,
    configured: HerdrLaunchEndpoint,
) -> Result<Option<Hold>, Refusal> {
    require_local(&configured).map_err(Code::RemoteGateway.of())?;
    let records = records().await.map_err(Code::ConsentStore.of())?;
    let mut previous_hold = None;
    for consent in records.iter().filter(|record| {
        record.provider == provider.as_str()
            && record.key.node == configured.execution_node
            && record.key.socket == std::path::Path::new(&configured.socket_addr)
            && record.key.named_session == configured.herdr_session
    }) {
        let sink = custody::LiveSink::new(deps.shared.clone(), provider.clone(), &consent.key)
            .map_err(Code::Custody.of())?;
        let mut unreleased = Vec::new();
        for intent in consent.unresolved_intents().iter().filter(|intent| {
            intent.channel == request.channel_id.get()
                && (intent.source_message == request.user_msg_id.get()
                    || request
                        .source_message_ids
                        .iter()
                        .any(|id| id.get() == intent.source_message))
        }) {
            if !sink.released(intent).await.map_err(Code::Custody.of())? {
                unreleased.push(intent.clone());
            }
        }
        let current = Hold {
            key: consent.key.clone(),
            preview: consent.preview.clone(),
            owner: consent.owner,
            bot: consent.bot,
            unreleased,
        };
        if !current.unreleased.is_empty() {
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
        .ok_or_else(|| Code::HostedRow.of()("herdr admission row unavailable".into()))?;
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
                return Err(Code::HostedRow.of()("unknown hosted execution".into()));
            }
            _ => {}
        },
        HostedLookup::Missing => {}
        other => {
            return Err(Code::HostedRow.of()(format!(
                "herdr admission row: {other:?}"
            )));
        }
    }
    if let Some(held) = previous_hold {
        return Ok(Some(held));
    }
    let source = peer(&configured).map_err(Code::PeerSource.of())?;
    let (checked, observation) = observe(&configured, source.clone()).await;
    if checked.is_ok() {
        return Ok(None);
    }
    let observation = &observation;
    let refuse = |code: Code| move |detail: String| code.of()(detail).observed(observation);
    let prepared = preview::prepare(source, 1)
        .await
        .map_err(refuse(Code::Preview))?;
    let key = ConsentKey {
        node: configured.execution_node,
        socket: configured.socket_addr.into(),
        named_session: configured.herdr_session,
        // SAFETY: geteuid has no preconditions and cannot fail.
        user: unsafe { libc::geteuid() },
        config: prepared.logical().into(),
        target: prepared.target().into(),
    }
    .normalized()
    .map_err(refuse(Code::Preview))?;
    let previous = records.into_iter().find(|record| record.key == key);
    let mut token = prepared.token;
    if let Some(previous) = previous {
        token.revision = previous.preview.revision;
        if token != previous.preview {
            token.revision = token
                .revision
                .checked_add(1)
                .ok_or_else(|| refuse(Code::Preview)("consent revision exhausted".into()))?;
        }
    }
    let owner = deps
        .shared
        .settings
        .read()
        .await
        .owner_user_id
        .ok_or_else(|| refuse(Code::Owner)("configure an owner before herdr consent".into()))?;
    let bot = deps
        .http
        .get_current_user()
        .await
        .map_err(|e| refuse(Code::DiscordIdentity)(e.to_string()))?
        .id
        .get();
    Ok(Some(Hold {
        key,
        preview: token,
        owner,
        bot,
        unreleased: Vec::new(),
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
        Box::pin(run(deps.http, deps.shared, provider, consent, event, None)).await
    }
}

async fn mailbox_intents(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    request: &IntakeRequest,
) -> Result<Vec<IntentRef>, String> {
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
    wanted
        .into_iter()
        .map(|source| {
            let found = items
                .iter()
                .flat_map(|item| item.source_message_queued_generations())
                .find(|s| s.message_id == source)
                .ok_or("original source not durably queued; checkpoint remains unread")?;
            Ok(IntentRef {
                store: "mailbox".into(),
                id: format!("{}:{}", source.get(), found.queued_generation),
                provider: provider.as_str().into(),
                channel: channel.get(),
                source_message: source.get(),
                fifo: found.queued_generation,
            })
        })
        .collect()
}

async fn register_intents(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    hold: Hold,
    intents: Vec<IntentRef>,
    explicit: bool,
) -> Result<(), String> {
    let consent = hold.record(intents).await?;
    // Card failure never relinquishes custody or retries a send whose response was lost.
    if let Err(error) = hold.effect(deps, provider, consent, explicit).await {
        tracing::warn!(%error, "consent card held after durable input");
    }
    Ok(())
}

pub(in crate::services::discord::router) fn register_mailbox<'a, 'b: 'a>(
    deps: &'a IntakeDeps<'b>,
    provider: &'a ProviderKind,
    request: &'a IntakeRequest,
    hold: Hold,
    explicit: bool,
) -> impl std::future::Future<Output = Result<(), String>> + Send + 'a {
    Box::pin(async move {
        let intents = mailbox_intents(deps, provider, request).await?;
        register_intents(deps, provider, hold, intents, explicit).await
    })
}

pub(in crate::services::discord::router) async fn restore_then_register(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    request: &IntakeRequest,
    hold: Result<Hold, Refusal>,
    restore: impl std::future::Future<Output = super::super::IntakeExecution> + Send,
) -> super::super::IntakeExecution {
    let outcome = Box::pin(restore).await;
    if outcome == super::super::IntakeExecution::Deferred {
        let registered = async {
            let hold =
                hold.map_err(|refusal| format!("{}: {}", refusal.code(), refusal.detail()))?;
            let intents = mailbox_intents(deps, provider, request).await?;
            // Backstop still restores the original head and lease, but repeats no card/store effect.
            let already_held = intents
                .iter()
                .all(|intent| hold.unreleased.contains(intent));
            if !already_held {
                register_intents(deps, provider, hold, intents, false).await?;
            }
            Ok::<_, String>(())
        }
        .await;
        if let Err(error) = registered {
            tracing::warn!(%error, "restored consent queue remains held");
        }
    }
    outcome
}

// Keep the custody enqueue future out of callers that take the unchanged Legacy branch.
pub(in crate::services::discord::router) fn hold_live<'a, 'b: 'a>(
    deps: &'a IntakeDeps<'b>,
    mut submission: IntakeSubmission,
    hold: Result<Hold, Refusal>,
) -> impl std::future::Future<Output = Result<(), Error>> + Send + 'a {
    Box::pin(async move {
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
            return Err(
                "herdr consent durable custody unavailable; checkpoint remains unread".into(),
            );
        }
        // A slash command posts its error text, so only the refusal's public form leaves here.
        let hold = match hold {
            Ok(hold) => hold,
            Err(refusal) => {
                let (code, detail) = (refusal.code(), refusal.detail());
                tracing::warn!(code, detail, explicit, "consent input remains held");
                if explicit {
                    notify_unverified(deps, &request, &refusal).await;
                }
                return Err(refusal.to_string().into());
            }
        };
        register_mailbox(deps, &provider, &request, hold, explicit)
            .await
            .map_err(|detail| {
                let refusal = Code::Record.of()(detail);
                tracing::warn!(detail = refusal.detail(), "consent hold record unverified");
                refusal.to_string().into()
            })
    })
}

/// A held input with no consent card names its cause once per original source and cause.
async fn notify_unverified(deps: &IntakeDeps<'_>, request: &IntakeRequest, refusal: &Refusal) {
    let (channel, source) = (request.channel_id, request.user_msg_id);
    if let Err(notice_error) =
        crate::services::discord::outbound::serenity_reference::send_referenced_lifecycle_notice(
            deps.http.clone(),
            deps.shared.clone(),
            channel,
            source,
            &refusal.notice(),
            format!(
                "herdr-consent-unverified:{}:{}",
                channel.get(),
                source.get()
            ),
            refusal.semantic(),
        )
        .await
    {
        let code = refusal.code();
        tracing::warn!(%notice_error, code, "unverified consent hold notice failed");
    }
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
    let mut proof = proof;
    let row_id = proof
        .as_ref()
        .map(|proof| proof.row().id)
        .or(request.intake_outbox_id);
    let mut recording = false;
    let result = async {
        let hold = held?;
        let handoff = |detail: &str| Code::Record.of()(detail.into());
        let row = proof
            .as_ref()
            .ok_or_else(|| handoff("consent requires an exact before-effects worker handoff"))?
            .row();
        let intent = IntentRef {
            store: "intake_outbox".into(),
            id: row.id.to_string(),
            provider: row.provider.clone(),
            channel: request.channel_id.get(),
            source_message: request.user_msg_id.get(),
            fifo: row.attempt_no as u64,
        };
        let pool = deps
            .shared
            .pg_pool
            .as_ref()
            .ok_or_else(|| handoff("worker custody database unavailable"))?;
        #[cfg(test)]
        tests::before_record().await;
        // Any record attempt may have published; only earlier failures retain rollback authority.
        recording = true;
        let consent = hold
            .record(vec![intent.clone()])
            .await
            .map_err(Code::Record.of())?;
        crate::db::intake_outbox::consent_handoff::hold_before_effects(
            pool,
            proof
                .take()
                .ok_or_else(|| handoff("before-effects authority missing"))?,
            &store(),
            &consent,
            &intent,
        )
        .await
        .map_err(Code::Record.of())?;
        hold.effect(deps, provider, consent, false)
            .await
            .map_err(Code::Card.of())
    }
    .await;
    if let Err(error) = result {
        let mut rearmed = false;
        if !recording
            && let Some(proof) = proof.take()
            && let Some(pool) = deps.shared.pg_pool.as_ref()
        {
            match crate::db::intake_outbox::consent_handoff::return_before_record(pool, proof).await
            {
                Ok(changed) => rearmed = changed,
                Err(rollback) => {
                    tracing::warn!(%rollback, "consent before-record rollback unverified")
                }
            }
        }
        let (code, detail) = (error.code(), error.detail());
        tracing::warn!(
            code,
            detail,
            ?row_id,
            rearmed,
            "worker consent keeps original payload deferred"
        );
        let state = if rearmed {
            "Pending; same original input will be checked again"
        } else {
            "Spawned; original input preserved; consent recovery requires inspection"
        };
        if !rearmed && let Some(row) = row_id {
            let notice = format!("worker 입력 보류: row {row} ({state}): {error}");
            if let Err(notice_error) = crate::services::discord::outbound::serenity_reference::send_referenced_lifecycle_notice(
                deps.http.clone(), deps.shared.clone(), request.channel_id, request.user_msg_id,
                &notice, format!("herdr-consent-worker:{row}"), format!("held:{rearmed}"),
            ).await {
                tracing::warn!(%notice_error, "worker consent lifecycle notice failed");
            }
        }
    }
    true
}

#[derive(Debug)]
pub(in crate::services::discord::router) struct QueuedHold(pub Result<Hold, Refusal>);
impl std::fmt::Display for QueuedHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("consent awaits original head restoration")
    }
}
impl std::error::Error for QueuedHold {}

#[cfg(test)]
#[path = "admission_tests.rs"]
pub(in crate::services::discord::router) mod tests;

#[cfg(test)]
#[path = "admission_notice_tests.rs"]
mod notice_tests;
