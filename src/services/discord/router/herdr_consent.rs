//! Owner consent operates existing custody; intake producers are wired separately.

pub(super) mod admission;
pub(super) mod custody;
mod return_input;
pub(in crate::services::discord) mod startup;
mod transport;

use crate::config::session_hosts::{self, EndpointMode};
use crate::services::discord::{Data, Error, SharedData};
use crate::services::herdr_launch::HerdrLaunchEndpoint;
use crate::services::provider::ProviderKind;
use crate::services::session_host::policy::{
    PeerSource,
    consent::{
        self, Consent, ConsentStore, State,
        engine::{Engine, Event},
        preview,
    },
};
use poise::serenity_prelude as serenity;
use std::sync::Arc;

pub(in crate::services::discord) fn store() -> ConsentStore {
    ConsentStore::new(
        crate::config::runtime_root().map(|r| r.join("runtime/herdr_policy/consents")),
    )
}

/// This pure guard precedes custom-ID parsing, command policy and every store lookup.
pub(in crate::services::discord) fn endpoint(channel: u64) -> Option<HerdrLaunchEndpoint> {
    session_hosts::with_boot(|boot| {
        let boot = boot?;
        let mapped = boot.herdr_endpoint(channel)?;
        let configured = boot.config().herdr.endpoints.get(&mapped.key)?;
        (configured.mode == EndpointMode::UserDefault).then(|| HerdrLaunchEndpoint {
            execution_node: mapped.execution_node.clone(),
            config_key: mapped.key.clone(),
            socket_addr: mapped.socket_path.to_string_lossy().into_owned(),
            herdr_session: mapped.herdr_session.clone(),
        })
    })
}

fn require_local(endpoint: &HerdrLaunchEndpoint) -> Result<(), String> {
    local_gateway(&endpoint.execution_node, "retry")
}

fn local_gateway(execution_node: &str, command: &str) -> Result<(), String> {
    if session_hosts::local_node().as_deref() != Some(execution_node) {
        return Err(format!(
            "실행 노드 {execution_node}의 gateway에서 !herdr consent {command}를 실행하세요. 현재 gateway는 원격 apply와 입력 해제를 전달하지 않습니다."
        ));
    }
    Ok(())
}

fn source(consent: &Consent) -> Result<Arc<dyn PeerSource>, String> {
    let endpoint =
        endpoint(consent.channel).ok_or("user-default consent endpoint is unavailable")?;
    if endpoint.execution_node != consent.key.node
        || endpoint.socket_addr.as_str() != consent.key.socket.to_string_lossy().as_ref()
        || endpoint.herdr_session != consent.key.named_session
    {
        return Err("consent endpoint changed; preserve held inputs".into());
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    if consent.key.user != unsafe { libc::geteuid() } {
        return Err("consent execution user changed; preserve held inputs".into());
    }
    peer(&endpoint)
}

fn peer(endpoint: &HerdrLaunchEndpoint) -> Result<Arc<dyn PeerSource>, String> {
    require_local(endpoint)?;
    #[cfg(test)]
    if let Some(source) = TEST_SOURCE.with(|source| source.borrow().clone()) {
        return Ok(source);
    }
    crate::services::session_host::herdr_endpoints()
        .launch_host()
        .and_then(|host| host.consent_source(endpoint))
        .ok_or_else(|| "consent peer source unavailable".into())
}

async fn records() -> Result<Vec<Consent>, String> {
    #[cfg(test)]
    STORE_READS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    tokio::task::spawn_blocking(|| store().load_all())
        .await
        .map_err(|e| format!("consent inventory: {e}"))?
}

async fn run(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    consent: Consent,
    event: Event<'_>,
    prepared: Option<preview::Prepared>,
) -> Result<(), String> {
    let source = source(&consent)?;
    let mut owner = transport::LiveOwner {
        http: http.clone(),
        shared: shared.clone(),
        provider: provider.clone(),
        source: source.clone(),
    };
    let mut cards = transport::LiveCards {
        http: http.clone(),
        channel: serenity::ChannelId::new(consent.channel),
    };
    let mut sink = custody::LiveSink::new(shared.clone(), provider.clone(), &consent.key)?;
    Engine {
        store: &store(),
        local_node: session_hosts::local_node().unwrap_or_default(),
        source,
        owner: &mut owner,
        cards: &mut cards,
        custody: &mut sink,
        boot: &startup::BOOT,
    }
    .handle(event, prepared)
    .await
    .map(|_| ())
}

pub(super) async fn component(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<bool, Error> {
    let Some(configured) = endpoint(component.channel_id.get()) else {
        return Ok(false);
    };
    let Some(click) = consent::card::parse(&component.data.custom_id) else {
        return Ok(false);
    };
    component
        .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
        .await?;
    let result = async {
        if !crate::services::discord::check_owner(component.user.id, &data.shared).await {
            return Err("현재 owner만 herdr 동의를 결정할 수 있습니다.".into());
        }
        require_local(&configured)?;
        let consent = records()
            .await?
            .into_iter()
            .find(|record| record.nonce == click.nonce)
            .ok_or("consent card is stale or missing; preserve held inputs")?;
        if consent.channel != component.channel_id.get()
            || consent.message != component.message.id.get()
            || consent.provider != data.provider.as_str()
            || consent.bot != ctx.cache.current_user().id.get()
            || component.message.author.id.get() != consent.bot
        {
            return Err("consent card identity changed; preserve held inputs".into());
        }
        let peer = source(&consent)?;
        let mut owner = transport::LiveOwner {
            http: ctx.http.clone(),
            shared: data.shared.clone(),
            provider: data.provider.clone(),
            source: peer.clone(),
        };
        use consent::engine::Owner;
        let current = owner.current(&consent).await?;
        let prepared = if click.yes && consent.state == State::Pending {
            Some(preview::prepare(peer, consent.preview.revision).await?)
        } else {
            None
        };
        let event = Event::Decide {
            expected: Box::new(consent.clone()),
            current: Box::new(current),
            actor: component.user.id.get(),
            yes: click.yes,
            now: now(),
        };
        run(
            &ctx.http,
            &data.shared,
            &data.provider,
            consent,
            event,
            prepared,
        )
        .await
    }
    .await;
    if let Err(error) = result {
        component
            .create_followup(
                ctx,
                serenity::CreateInteractionResponseFollowup::new()
                    .content(error)
                    .ephemeral(true),
            )
            .await?;
    }
    Ok(true)
}

/// Owner consent commands; `return` also closes returns in the legacy channel it moved to.
pub(in crate::services::discord) async fn retry(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    data: &Data,
    channel: serenity::ChannelId,
    text: &str,
) -> Result<bool, Error> {
    if text.split_whitespace().eq(["!herdr", "consent", "return"]) {
        return return_input::handle(ctx, msg, data, channel).await;
    }
    let Some(configured) = endpoint(channel.get()) else {
        return Ok(false);
    };
    if !text.split_whitespace().eq(["!herdr", "consent", "retry"]) {
        return Ok(false);
    }
    let result = async {
        if !crate::services::discord::check_owner(msg.author.id, &data.shared).await {
            return Err("현재 owner만 !herdr consent retry를 실행할 수 있습니다.".into());
        }
        require_local(&configured)?;
        let settings = data.shared.settings.read().await.clone();
        crate::services::discord::validate_live_channel_routing(
            ctx,
            &data.provider,
            &settings,
            channel,
        )
        .await
        .map_err(|_| "consent retry provider/channel binding changed".to_string())?;
        let load = || async {
            Ok::<Vec<_>, String>(
                records()
                    .await?
                    .into_iter()
                    .filter(|record| {
                        record.provider == data.provider.as_str()
                            && record
                                .intents
                                .iter()
                                .any(|intent| intent.channel == channel.get())
                    })
                    .collect(),
            )
        };
        let mut matching = load().await?;
        if matching.is_empty() {
            if data
                .shared
                .mailbox(channel)
                .snapshot()
                .await
                .intervention_queue
                .is_empty()
            {
                return Err(
                    "보류한 herdr 동의가 없습니다. 입력을 새로 만들거나 실행하지 않았습니다."
                        .into(),
                );
            }
            // Admission held these without a record; the drain re-checks the same head first.
            crate::services::discord::queue_io::kick_idle_queue_channel_if_context_available(
                &data.shared,
                &data.provider,
                channel,
                "herdr_consent_retry",
            )
            .await;
            matching = load().await?;
        }
        for consent in matching {
            crate::services::discord::validate_live_channel_routing(
                ctx,
                &data.provider,
                &settings,
                serenity::ChannelId::new(consent.channel),
            )
            .await
            .map_err(|_| "consent card provider/channel binding changed".to_string())?;
            let peer = source(&consent)?;
            // Replaying an Applied receipt needs no fresh evidence; a new generation's card does.
            let prepared = if consent.state != State::Applied || consent.needs_new_generation() {
                let revision = consent.preview.revision.checked_add(1);
                let revision = revision.ok_or("consent revision exhausted")?;
                Some(preview::prepare(peer, revision).await?)
            } else {
                None
            };
            let intent = consent
                .unresolved_intents()
                .into_iter()
                .find(|intent| intent.channel == channel.get())
                .or_else(|| {
                    consent
                        .intents
                        .iter()
                        .find(|intent| intent.channel == channel.get())
                        .cloned()
                })
                .ok_or("original custody reference is unavailable")?;
            let owner = data.shared.settings.read().await.owner_user_id;
            let bot = ctx
                .http
                .get_current_user()
                .await
                .map_err(|e| e.to_string())?
                .id
                .get();
            if consent.bot != bot || consent.provider != data.provider.as_str() {
                return Err("consent bot/provider changed".into());
            }
            let question = consent::Question {
                key: consent.key.clone(),
                preview: prepared.as_ref().map_or_else(
                    || consent.preview.clone(),
                    |prepared| prepared.token.clone(),
                ),
                owner: owner.ok_or("owner is not configured")?,
                bot,
                provider: data.provider.as_str().into(),
                channel: consent.channel,
                message: 0,
                intent,
            };
            let event = Event::Retry {
                question: Box::new(question),
                actor: msg.author.id.get(),
                owner,
                local_node: session_hosts::local_node().unwrap_or_default(),
                now: now(),
            };
            run(
                &ctx.http,
                &data.shared,
                &data.provider,
                consent,
                event,
                prepared,
            )
            .await?;
        }
        Ok::<(), String>(())
    }
    .await;
    if let Err(error) = result {
        msg.reply(&ctx.http, error).await?;
    }
    Ok(true)
}

/// Which Applied consents one replay pass owns.
#[derive(Clone, Copy)]
pub(super) enum Replay<'a> {
    /// Consents with no releasable input in these still-held channels.
    Excluding(&'a std::collections::BTreeSet<u64>),
    /// Consents with releasable input in `channel` and none in a still-held channel.
    Only {
        channel: u64,
        held: &'a std::collections::BTreeSet<u64>,
    },
}

impl Replay<'_> {
    fn owns(self, consent: &Consent) -> bool {
        let channels: Vec<u64> = consent
            .releasable_intents()
            .iter()
            .map(|intent| intent.channel)
            .collect();
        let (held, required) = match self {
            Self::Excluding(held) => (held, None),
            Self::Only { channel, held } => (held, Some(channel)),
        };
        !channels.iter().any(|channel| held.contains(channel))
            && required.is_none_or(|channel| channels.contains(&channel))
    }
}

pub(super) async fn recover(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    scope: Replay<'_>,
) -> Result<(), String> {
    let local = session_hosts::local_node();
    let enabled = session_hosts::with_boot(|boot| {
        boot.is_some_and(|boot| {
            boot.config().herdr.endpoints.values().any(|e| {
                e.mode == EndpointMode::UserDefault
                    && Some(e.execution_node.trim()) == local.as_deref()
            })
        })
    });
    if !enabled {
        return Ok(());
    }
    let mut failures = Vec::new();
    let records = tokio::task::spawn_blocking(|| store().load_each())
        .await
        .map_err(|e| e.to_string())??;
    for record in records {
        let consent = match record {
            Ok(consent) => consent,
            Err(error) => {
                failures.push(error);
                continue;
            }
        };
        if consent.provider != provider.as_str()
            || Some(&consent.key.node) != local.as_ref()
            || consent.state != State::Applied
        {
            continue;
        }
        if !scope.owns(&consent) {
            continue;
        }
        let bot = match http.get_current_user().await {
            Ok(bot) => bot.id.get(),
            Err(error) => {
                failures.push(error.to_string());
                continue;
            }
        };
        if consent.bot != bot {
            continue;
        }
        let event = Event::Recover(consent.key.clone());
        if let Err(error) = run(http, shared, provider, consent, event, None).await {
            failures.push(error);
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
#[path = "herdr_consent/service_tests.rs"]
mod service_tests;

#[cfg(test)]
static STORE_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
thread_local! {
    static TEST_SOURCE: std::cell::RefCell<Option<Arc<dyn PeerSource>>> = const { std::cell::RefCell::new(None) };
}
