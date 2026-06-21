//! Native `/sidecar` slash-command interaction handling.
//!
//! Deterministic (no AI turn): `/sidecar` posts an ephemeral card with two
//! string-select dropdowns (host Mac + target device) plus 연결/해제 buttons.
//! Selecting the host Mac re-queries that Mac's connectable devices and
//! re-renders the device dropdown, so the list reflects whichever Mac is
//! chosen and refreshes on each (re)selection. Selections are stored in an
//! in-memory pending map keyed by the card's message id; on button click the
//! chosen `SidecarLauncher` action runs either locally (mac-book — the
//! dcserver host) or over SSH (mac-mini).

use std::process::Stdio;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;

use super::{Data, Error, check_auth};

/// Host Mac selected by default when the card is first posted (dcserver host).
pub(in crate::services::discord) const SIDECAR_DEFAULT_MAC: &str = "mac-book";

/// How long a posted picker stays interactive before it is considered stale.
const SIDECAR_PENDING_TTL: Duration = Duration::from_secs(30 * 60);
/// Hard cap on how long a connect/disconnect (incl. SSH) may run.
const SIDECAR_ACTION_TIMEOUT: Duration = Duration::from_secs(20);
/// Cap on the device-listing probe. Generous enough for the mac-mini SSH probe;
/// the local mac-book probe returns near-instantly so it never approaches this.
const SIDECAR_DEVICES_TIMEOUT: Duration = Duration::from_secs(8);

struct SidecarPending {
    mac: Option<String>,
    device: Option<String>,
    owner: serenity::UserId,
    updated_at: Instant,
}

static SIDECAR_PENDING: LazyLock<dashmap::DashMap<serenity::MessageId, SidecarPending>> =
    LazyLock::new(dashmap::DashMap::new);

/// Record a freshly-posted picker so its dropdown selections can be tracked.
/// `mac` is the host pre-selected at post time (the device list was queried for it).
pub(in crate::services::discord) fn remember_sidecar_pending(
    message_id: serenity::MessageId,
    owner: serenity::UserId,
    mac: Option<String>,
) {
    SIDECAR_PENDING.insert(
        message_id,
        SidecarPending {
            mac,
            device: None,
            owner,
            updated_at: Instant::now(),
        },
    );
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/Users/itismyfield".to_string())
}

/// List Sidecar-connectable devices as seen by the given host Mac.
/// `mac-book` → local binary; `mac-mini` → over SSH (key/agent auth).
/// Returns empty on timeout/error; the caller falls back to a static option.
pub(in crate::services::discord) async fn list_sidecar_devices_on(mac: &str) -> Vec<String> {
    let mut cmd = if mac == "mac-mini" {
        let mut c = tokio::process::Command::new("/usr/bin/ssh");
        c.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=8",
            "-o",
            "ServerAliveInterval=3",
            "-o",
            "ServerAliveCountMax=3",
            "mac-mini",
            "~/bin/SidecarLauncher devices",
        ]);
        c
    } else {
        let bin = format!("{}/bin/SidecarLauncher", home_dir());
        let mut c = tokio::process::Command::new(bin);
        c.arg("devices");
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = match tokio::time::timeout(SIDECAR_DEVICES_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => out,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .take(25)
        .collect()
}

fn select_option(
    label: &str,
    value: &str,
    selected: Option<&str>,
) -> serenity::CreateSelectMenuOption {
    let option = serenity::CreateSelectMenuOption::new(label, value);
    if selected == Some(value) {
        option.default_selection(true)
    } else {
        option
    }
}

/// Build the picker components: host-Mac dropdown, device dropdown, 연결/해제
/// buttons. `selected_mac`/`selected_device` mark the matching option as the
/// default so a re-rendered card keeps the user's visible choice.
pub(in crate::services::discord) fn build_sidecar_components(
    devices: &[String],
    selected_mac: Option<&str>,
    selected_device: Option<&str>,
) -> Vec<serenity::CreateActionRow> {
    let mac_menu = serenity::CreateSelectMenu::new(
        "sidecar:mac",
        serenity::CreateSelectMenuKind::String {
            options: vec![
                select_option("mac-book (이 머신)", "mac-book", selected_mac),
                select_option("mac-mini", "mac-mini", selected_mac),
            ],
        },
    )
    .placeholder("호스트 Mac 선택")
    .min_values(1)
    .max_values(1);

    let device_options: Vec<serenity::CreateSelectMenuOption> = if devices.is_empty() {
        vec![select_option("Oh의 iPad", "Oh의 iPad", selected_device)]
    } else {
        devices
            .iter()
            .map(|d| select_option(d.as_str(), d.as_str(), selected_device))
            .collect()
    };
    let device_menu = serenity::CreateSelectMenu::new(
        "sidecar:device",
        serenity::CreateSelectMenuKind::String {
            options: device_options,
        },
    )
    .placeholder("연결할 기기 선택")
    .min_values(1)
    .max_values(1);

    let connect = serenity::CreateButton::new("sidecar:connect")
        .label("연결")
        .style(serenity::ButtonStyle::Success);
    let disconnect = serenity::CreateButton::new("sidecar:disconnect")
        .label("해제")
        .style(serenity::ButtonStyle::Secondary);

    vec![
        serenity::CreateActionRow::SelectMenu(mac_menu),
        serenity::CreateActionRow::SelectMenu(device_menu),
        serenity::CreateActionRow::Buttons(vec![connect, disconnect]),
    ]
}

pub(super) fn is_sidecar_custom_id(custom_id: &str) -> bool {
    custom_id.starts_with("sidecar:")
}

fn selected_string_value(component: &serenity::ComponentInteraction) -> Option<String> {
    match &component.data.kind {
        serenity::ComponentInteractionDataKind::StringSelect { values } => values.first().cloned(),
        _ => None,
    }
}

async fn ephemeral_reply(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    message: impl Into<String>,
) -> Result<(), Error> {
    component
        .create_response(
            ctx,
            serenity::CreateInteractionResponse::Message(
                serenity::CreateInteractionResponseMessage::new()
                    .content(message)
                    .ephemeral(true),
            ),
        )
        .await?;
    Ok(())
}

/// Run `SidecarLauncher <action> "<device>"` on the chosen Mac.
/// `mac-book` → local binary; `mac-mini` → over SSH (key/agent auth).
/// Returns `(success, detail)` where detail is trimmed stdout/stderr.
async fn run_sidecar_action(mac: &str, action: &str, device: &str) -> (bool, String) {
    let mut cmd = if mac == "mac-mini" {
        let remote = format!("~/bin/SidecarLauncher {action} \"{device}\"");
        let mut c = tokio::process::Command::new("/usr/bin/ssh");
        c.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=8",
            "-o",
            "ServerAliveInterval=3",
            "-o",
            "ServerAliveCountMax=3",
            "mac-mini",
            &remote,
        ]);
        c
    } else {
        let bin = format!("{}/bin/SidecarLauncher", home_dir());
        let mut c = tokio::process::Command::new(bin);
        c.arg(action).arg(device);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    match tokio::time::timeout(SIDECAR_ACTION_TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut detail = String::new();
            if !stdout.trim().is_empty() {
                detail.push_str(stdout.trim());
            }
            if !stderr.trim().is_empty() {
                if !detail.is_empty() {
                    detail.push('\n');
                }
                detail.push_str(stderr.trim());
            }
            (output.status.success(), detail)
        }
        Ok(Err(e)) => (false, format!("실행 실패: {e}")),
        Err(_) => (false, "시간 초과(20s)".to_string()),
    }
}

pub(super) async fn handle_sidecar_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let user_id = component.user.id;
    let user_name = &component.user.name;
    if !check_auth(user_id, user_name, &data.shared, &data.token).await {
        return ephemeral_reply(ctx, component, "Not authorized for this bot.").await;
    }

    let message_id = component.message.id;
    let action = component
        .data
        .custom_id
        .strip_prefix("sidecar:")
        .unwrap_or("");

    let Some((owner, updated_at)) = SIDECAR_PENDING
        .get(&message_id)
        .map(|entry| (entry.owner, entry.updated_at))
    else {
        return ephemeral_reply(
            ctx,
            component,
            "이 Sidecar 패널이 만료됐습니다. `/sidecar`를 다시 실행하세요.",
        )
        .await;
    };

    if Instant::now().duration_since(updated_at) > SIDECAR_PENDING_TTL {
        SIDECAR_PENDING.remove(&message_id);
        return ephemeral_reply(
            ctx,
            component,
            "이 Sidecar 패널이 만료됐습니다. `/sidecar`를 다시 실행하세요.",
        )
        .await;
    }

    if owner != user_id {
        return ephemeral_reply(ctx, component, "패널을 연 사용자만 조작할 수 있습니다.").await;
    }

    match action {
        "mac" => {
            let Some(mac) = selected_string_value(component) else {
                component
                    .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
                    .await?;
                return Ok(());
            };
            // Switching host invalidates the previously-picked device, since the
            // device list is re-queried per Mac.
            if let Some(mut state) = SIDECAR_PENDING.get_mut(&message_id) {
                state.mac = Some(mac.clone());
                state.device = None;
                state.updated_at = Instant::now();
            }
            // Defer the update (15-min window) so the per-Mac device probe (SSH
            // for mac-mini) can run without risking the 3-second ack timeout,
            // then re-render the device dropdown with that Mac's devices.
            component
                .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
                .await?;
            let devices = list_sidecar_devices_on(&mac).await;
            let components = build_sidecar_components(&devices, Some(&mac), None);
            component
                .edit_response(
                    ctx,
                    serenity::EditInteractionResponse::new().components(components),
                )
                .await?;
            Ok(())
        }
        "device" => {
            if let Some(value) = selected_string_value(component)
                && let Some(mut state) = SIDECAR_PENDING.get_mut(&message_id)
            {
                state.device = Some(value);
                state.updated_at = Instant::now();
            }
            // Acknowledge without changing the card; the client keeps the
            // visible selection and the value is now stored server-side.
            component
                .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
                .await?;
            Ok(())
        }
        "connect" | "disconnect" => {
            let (mac, device) = SIDECAR_PENDING
                .get(&message_id)
                .map(|s| (s.mac.clone(), s.device.clone()))
                .unwrap_or((None, None));
            let (Some(mac), Some(device)) = (mac, device) else {
                return ephemeral_reply(
                    ctx,
                    component,
                    "호스트 Mac과 기기를 모두 선택한 뒤 눌러주세요.",
                )
                .await;
            };

            // Defer the message update (15-min window) so the SSH/local action
            // can run without risking the 3-second interaction timeout.
            component
                .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
                .await?;

            let (ok, detail) = run_sidecar_action(&mac, action, &device).await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ◀ [{user_name}] /sidecar {action} mac={mac} device={device} ok={ok}"
            );

            let head = match (ok, action) {
                (true, "connect") => format!("✅ **{mac}** 에 `{device}` Sidecar 연결됨"),
                (true, _) => format!("✅ **{mac}** 에서 `{device}` Sidecar 해제됨"),
                (false, "connect") => format!("⛔ **{mac}** `{device}` 연결 실패"),
                (false, _) => format!("⛔ **{mac}** `{device}` 해제 실패"),
            };
            let body = if detail.is_empty() {
                head
            } else {
                format!("{head}\n```\n{detail}\n```")
            };

            component
                .edit_response(
                    ctx,
                    serenity::EditInteractionResponse::new()
                        .content(body)
                        .components(Vec::new()),
                )
                .await?;
            SIDECAR_PENDING.remove(&message_id);
            Ok(())
        }
        _ => ephemeral_reply(ctx, component, "알 수 없는 Sidecar 동작입니다.").await,
    }
}
