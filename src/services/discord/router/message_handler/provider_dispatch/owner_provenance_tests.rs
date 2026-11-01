//! A routine or scheduled start run through the legacy entry on one channel and the owner's drive
//! on another resolves the same role, session key, tmux session, channel, workspace and session.
use super::super::start_idle;
use crate::services::discord::SharedData;
use crate::services::discord::admin_host_guard::tests::{ModeTmux, Tmux, api_child};
use crate::services::discord::host_defer_gate::tests::{map_channel, postgres};
use crate::services::discord::host_teardown_gate::test_support::runtime;
use crate::services::discord::queue_io::input_owner::{admission, routine};
use crate::services::discord::router::message_handler::headless_turn::STARTS_RESOLVED;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;
use serde_json::{Value, json};
use std::sync::Arc;

const PROMPT: &str = "provenance start";
const LEGACY: ChannelId = ChannelId::new(1_479_671_302_387_075_001);
const OWNED: ChannelId = ChannelId::new(1_479_671_302_387_075_002);

/// One start as its caller hands it over; `side` names what differs between the two channels.
struct Start {
    source: &'static str,
    metadata: fn(&str) -> Value,
    label: Option<fn(&str) -> String>,
    dm_user: Option<u64>,
    /// Kept flat, as a scheduled seam kept it before its provenance had keys of its own.
    flat: bool,
}

/// The two runs' resolved choices with each channel's own names written as `SIDE` and `CH`.
async fn legacy_and_owned(start: Start) -> (String, String) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = ModeTmux::install();
    tmux.serve(Tmux::Live);
    stub_provider_clis();
    let (db, pool) = postgres().await;
    let (shared, registry) = runtime(&pool).await;
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    let discord = discord_mock::DiscordMockState::new();
    // Discord names the channels unlike their binding, so only a hint picks the binding's name.
    for channel in [LEGACY, OWNED] {
        discord.name_channel(channel.get(), "discord-named");
    }
    let (proxy, gateway, _server) = discord_mock::start(discord).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    shared.http.cached_serenity_ctx.set(ctx).unwrap();
    shared
        .http
        .cached_bot_token
        .set("test-token".into())
        .unwrap();
    let workdir = tempfile::tempdir().unwrap();
    // The agent binding a flat scheduled start's hint is read from.
    let bound = "INSERT INTO agents(id,name,provider,discord_channel_cc) VALUES('agent7272','A','claude','sideowned-agent')";
    if start.flat {
        sqlx::query(bound).execute(&pool).await.unwrap();
    }
    let mut seen = Vec::new();
    for (channel, side, user) in [
        (LEGACY, "sidelegacy", 7_272_001),
        (OWNED, "sideowned", 7_272_002),
    ] {
        known_channel(
            &shared,
            channel,
            side,
            start.dm_user.is_some() || start.flat,
            workdir.path(),
        )
        .await;
        let (metadata, label) = ((start.metadata)(side), start.label.map(|label| label(side)));
        let user = start.dm_user.map(|_| user);
        if channel == LEGACY {
            legacy(&registry, (start.source, metadata, label, user)).await;
        } else {
            let context = match start.flat {
                true => flat(metadata),
                false => kept(start.source, metadata, label, user, side),
            };
            // The stub CLI ends the turn after its start resolved; only the choices are compared.
            let _settled = owned(&shared, context).await;
        }
        let starts = STARTS_RESOLVED.lock().unwrap().clone();
        let resolved = starts.iter().rev().find(|(at, _)| *at == channel.get());
        let resolved = resolved.map(|(_, s)| s.clone()).unwrap_or_default();
        let resolved = resolved
            .replace(side, "SIDE")
            .replace(&channel.get().to_string(), "CH");
        let user = user.map(|user| user.to_string());
        seen.push(user.map_or(resolved.clone(), |user| resolved.replace(&user, "USER")));
    }
    db.drop().await;
    (seen.remove(0), seen.remove(0))
}

/// A channel the runtime already knows, with its workspace and a provider session to continue;
/// a nameless one (a DM, or a restored channel) is named by its start's hint.
async fn known_channel(
    shared: &SharedData,
    channel: ChannelId,
    side: &str,
    nameless: bool,
    dir: &std::path::Path,
) {
    map_channel(shared, channel, &format!("{side}-agent")).await;
    let mut core = shared.core.lock().await;
    let session = core.sessions.get_mut(&channel).unwrap();
    session.current_path = Some(dir.display().to_string());
    session.session_id = Some("sid".into());
    if nameless {
        session.channel_name = None;
    }
}

/// The legacy entry the routine or scheduled caller uses for an unowned channel.
async fn legacy(
    registry: &crate::services::discord::health::HealthRegistry,
    (source, metadata, label, user): (&str, Value, Option<String>, Option<u64>),
) {
    use crate::services::discord::health::{
        reserve_headless_agent_turn, start_reserved_headless_agent_turn_in_dm,
        start_reserved_headless_agent_turn_with_owner_channel,
    };
    let (claude, prompt, source) = (
        ProviderKind::Claude,
        PROMPT.to_string(),
        Some(source.into()),
    );
    let (metadata, reservation) = (Some(metadata), reserve_headless_agent_turn(LEGACY));
    let started = match user {
        Some(user) => {
            start_reserved_headless_agent_turn_in_dm(
                registry,
                LEGACY,
                LEGACY,
                user,
                claude,
                prompt,
                source,
                metadata,
                label,
                reservation,
            )
            .await
        }
        None => {
            let hint = Some("sidelegacy-agent".to_string());
            start_reserved_headless_agent_turn_with_owner_channel(
                registry,
                LEGACY,
                LEGACY,
                claude,
                prompt,
                source,
                metadata,
                hint,
                label,
                reservation,
            )
            .await
        }
    };
    assert!(started.is_ok(), "the legacy start: {started:?}");
}

/// The context the routine or scheduled owner seam keeps for the same start.
fn kept(
    source: &str,
    metadata: Value,
    label: Option<String>,
    user: Option<u64>,
    side: &str,
) -> Value {
    let hint = match user {
        Some(user) => format!("dm-{user}"),
        None => format!("{side}-agent"),
    };
    let mut kept = match source {
        "scheduled_message" => metadata.clone(),
        _ => json!({"origin":"routine","is_dm":user.is_some()}),
    };
    kept["metadata"] = metadata;
    (kept["channel_name_hint"], kept["synthetic_label"]) = (json!(hint), json!(label));
    (kept["source"], kept["is_dm"]) = (json!(source), json!(user.is_some()));
    kept
}

/// A scheduled start's context as the seam kept it before its provenance had keys of its own.
fn flat(metadata: Value) -> Value {
    let mut kept = metadata;
    (kept["provider"], kept["delivery_id"]) = (json!("claude"), json!("delivery-7272"));
    kept["origin"] = json!("scheduled");
    kept
}

/// The start admitted under its turn's ticket by the owned channel's owner before a restart, then
/// its front started by the restored owner's idle start.
async fn owned(shared: &Arc<SharedData>, context: Value) -> Result<(), String> {
    let mailbox = shared.mailbox(OWNED);
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let dir = dir.join("claude/provenance-owner");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}.json", OWNED.get()));
    let before = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
    let kept_by = before.handle(OWNED);
    kept_by
        .v2_adopt(path.clone(), "input-owner".into())
        .await
        .unwrap();
    // A process generation settles the synthetic original, as a running process has one.
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "5").unwrap();
    let turn = format!("discord:{}:9100000000000757002", OWNED.get());
    let (original, ticket) = routine::synthetic_original(&turn, OWNED.get(), PROMPT, 1)?;
    let admitted = admission::admit_with_context(&kept_by, &original, ticket, context).await;
    let admitted = match admitted {
        admission::Admission::Accepted(_) => Ok(()),
        admission::Admission::Pending(_) => Err("pending".to_string()),
        admission::Admission::Rejected(reason) => Err(format!("{reason:?}")),
    };
    assert_eq!(admitted, Ok(()), "the start is admitted");
    drop((kept_by, before));
    mailbox.v2_adopt(path, "input-owner".into()).await.unwrap();
    let granted = mailbox.v2_request(|slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        let front = owner.front().ok_or("no front")?;
        owner.grant_front(&front).map(|()| front)
    });
    start_idle(Arc::clone(shared), OWNED, granted.await?).await
}

/// A real provider CLI must not start if a regression reaches the launch.
fn stub_provider_clis() {
    let paths = std::env::var_os("PATH").unwrap();
    let stubs = std::env::split_paths(&paths).next().unwrap();
    for cli in ["claude", "codex"] {
        std::fs::write(stubs.join(cli), "#!/bin/sh\nexit 1\n").unwrap();
        let mode = std::os::unix::fs::PermissionsExt::from_mode(0o755);
        std::fs::set_permissions(stubs.join(cli), mode).unwrap();
    }
}

fn routine(strategy: &'static str) -> fn(&str) -> Value {
    match strategy {
        "fresh" => {
            |_| json!({"routine_id":"routine-7272","run_id":"run-7272","agent_id":"agent7272","execution_strategy":"fresh","is_dm":false})
        }
        "dm" => {
            |_| json!({"routine_id":"routine-7272","run_id":"run-7272","agent_id":"agent7272","execution_strategy":"persistent","is_dm":true})
        }
        _ => {
            |_| json!({"routine_id":"routine-7272","run_id":"run-7272","agent_id":"agent7272","execution_strategy":"persistent","is_dm":false})
        }
    }
}

/// A scheduled start on an immutable snapshot keys its session off the snapshot's label and
/// starts cold, on the owned channel as on the legacy one.
#[tokio::test]
async fn an_owned_scheduled_snapshot_start_resolves_as_its_legacy_start_pg() {
    let test = "services::discord::router::message_handler::provider_dispatch::owner_input::tests::provenance::an_owned_scheduled_snapshot_start_resolves_as_its_legacy_start_pg";
    if !api_child(test) {
        return;
    }
    let (legacy, owned) = legacy_and_owned(Start {
        source: "scheduled_message",
        metadata: |side| {
            json!({"agent_id":"agent7272","scheduled_message_id":"sm-7272",
            "scheduled_snapshot_session_label":format!("snap-{side}")})
        },
        label: Some(|side| format!("snap-{side}")),
        dm_user: None,
        flat: false,
    })
    .await;
    assert!(
        legacy.contains("snap-SIDE") && legacy.ends_with("None"),
        "{legacy}"
    );
    assert_eq!(
        owned, legacy,
        "same session key, tmux session, workspace and cold start"
    );
}

/// A routine with its synthetic label runs in that label's tmux session under its agent's role.
#[tokio::test]
async fn an_owned_labelled_routine_start_resolves_as_its_legacy_start_pg() {
    let test = "services::discord::router::message_handler::provider_dispatch::owner_input::tests::provenance::an_owned_labelled_routine_start_resolves_as_its_legacy_start_pg";
    if !api_child(test) {
        return;
    }
    let (legacy, owned) = legacy_and_owned(Start {
        source: "routine",
        metadata: routine("persistent"),
        label: Some(|side| format!("routine-{side}")),
        dm_user: None,
        flat: false,
    })
    .await;
    assert!(
        legacy.contains("routine-SIDE") && legacy.contains("agent7272"),
        "{legacy}"
    );
    assert_eq!(
        owned, legacy,
        "same role, tmux session, workspace and kept session"
    );
}

/// A DM routine takes the user's `dm-<user>` channel as its channel and tmux session.
#[tokio::test]
async fn an_owned_dm_routine_start_resolves_as_its_legacy_start_pg() {
    let test = "services::discord::router::message_handler::provider_dispatch::owner_input::tests::provenance::an_owned_dm_routine_start_resolves_as_its_legacy_start_pg";
    if !api_child(test) {
        return;
    }
    let (legacy, owned) = legacy_and_owned(Start {
        source: "routine",
        metadata: routine("dm"),
        label: None,
        dm_user: Some(0),
        flat: false,
    })
    .await;
    assert!(legacy.contains("dm-USER"), "{legacy}");
    assert_eq!(
        owned, legacy,
        "same channel, tmux session, role and workspace"
    );
}

/// A fresh routine never continues the channel's provider session.
#[tokio::test]
async fn an_owned_fresh_routine_start_resolves_as_its_legacy_start_pg() {
    let test = "services::discord::router::message_handler::provider_dispatch::owner_input::tests::provenance::an_owned_fresh_routine_start_resolves_as_its_legacy_start_pg";
    if !api_child(test) {
        return;
    }
    let (legacy, owned) = legacy_and_owned(Start {
        source: "routine",
        metadata: routine("fresh"),
        label: Some(|side| format!("fresh-{side}")),
        dm_user: None,
        flat: false,
    })
    .await;
    assert!(
        legacy.contains("fresh-SIDE") && legacy.ends_with("None"),
        "{legacy}"
    );
    assert_eq!(
        owned, legacy,
        "same session key, tmux session and cold start"
    );
}

/// A scheduled start kept flat before a restart starts as its legacy start: the snapshot's session
/// key and tmux session, cold, as a scheduled message.
#[tokio::test]
async fn an_owned_flat_scheduled_start_kept_before_a_restart_resolves_as_its_legacy_start_pg() {
    let test = "services::discord::router::message_handler::provider_dispatch::owner_input::tests::provenance::an_owned_flat_scheduled_start_kept_before_a_restart_resolves_as_its_legacy_start_pg";
    if !api_child(test) {
        return;
    }
    let (legacy, owned) = legacy_and_owned(Start {
        source: "scheduled_message",
        metadata: |side| {
            json!({"agent_id":"agent7272","scheduled_message_id":"sm-7272",
            "scheduled_snapshot_session_label":format!("snap-{side}")})
        },
        label: Some(|side| format!("snap-{side}")),
        dm_user: None,
        flat: true,
    })
    .await;
    let scheduled = legacy.contains("snap-SIDE") && legacy.contains("scheduled_message");
    assert!(scheduled && legacy.ends_with("None"), "{legacy}");
    let bound = legacy.contains("channel_name: Some(\"SIDE-agent\")");
    assert!(
        bound && !legacy.contains("discord-named"),
        "the binding names it: {legacy}"
    );
    assert_eq!(
        owned, legacy,
        "same session key, tmux session, channel, source and cold start"
    );
}
