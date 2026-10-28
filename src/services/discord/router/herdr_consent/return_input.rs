//! `!herdr consent return` reserves held inputs for this boot's refused legacy rollback; in the
//! legacy channel it later closes the returns its custody no longer holds.

use super::*;
use crate::config::session_hosts::migration::binding_digest;
use consent::{IntentRef, settlement::Resolution};

pub(super) async fn handle(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    data: &Data,
    channel: serenity::ChannelId,
) -> Result<bool, Error> {
    let Some(mapped) = session_hosts::herdr_endpoint(channel.get()) else {
        return Ok(false);
    };
    let result = async {
        if !crate::services::discord::check_owner(msg.author.id, &data.shared).await {
            return Err("현재 owner만 !herdr consent return을 실행할 수 있습니다.".to_string());
        }
        local_gateway(&mapped.execution_node, "return")?;
        let settings = data.shared.settings.read().await.clone();
        crate::services::discord::validate_live_channel_routing(
            ctx,
            &data.provider,
            &settings,
            channel,
        )
        .await
        .map_err(|_| "consent return provider/channel binding changed".to_string())?;
        match endpoint(channel.get()) {
            Some(_) => reserve(data, channel.get()).await,
            None => close(data, channel.get()).await,
        }
    }
    .await;
    crate::services::discord::http::send_channel_message_reply(
        &ctx.http,
        msg,
        result.unwrap_or_else(|error| error),
    )
    .await?;
    Ok(true)
}

/// This provider's consents with unresolved input in `channel`, and that input.
async fn held(data: &Data, channel: u64) -> Result<Vec<(Consent, Vec<IntentRef>)>, String> {
    let mut held = Vec::new();
    for consent in records().await? {
        let intents: Vec<_> = consent
            .unresolved_intents()
            .into_iter()
            .filter(|intent| intent.channel == channel)
            .collect();
        if intents.is_empty() {
            continue;
        }
        if consent.provider != data.provider.as_str()
            || intents
                .iter()
                .any(|intent| intent.store != "mailbox" || intent.provider != consent.provider)
        {
            return Err("이 채널의 보류 입력 중 mailbox 밖이나 다른 provider의 입력은 반환할 수 없습니다. 상태를 바꾸지 않았습니다.".into());
        }
        held.push((consent, intents));
    }
    Ok(held)
}

async fn settle(consent: Consent, changes: Vec<(IntentRef, Resolution)>) -> Result<(), String> {
    tokio::task::spawn_blocking(move || store().settle(&consent, &changes).map(|_| ()))
        .await
        .map_err(|e| format!("consent return: {e}"))?
}

/// The candidate binding this boot refused only because of this channel's held input.
fn rollback_target(channel: u64) -> Result<([u8; 32], String), String> {
    let config = crate::config::load().map_err(|e| format!("현재 설정을 읽지 못했습니다: {e}"))?;
    session_hosts::with_boot_migration(|metadata| {
        let metadata = metadata.ok_or("이번 부팅의 session_hosts 후보가 없습니다.")?;
        if metadata.restart_required(&config) {
            return Err(
                "부팅 뒤 session_hosts 설정이 바뀌었습니다. 재시작한 뒤 다시 실행하세요.".into(),
            );
        }
        let candidate = metadata.candidate();
        let key = candidate
            .herdr
            .channels
            .iter()
            .find_map(|(raw, key)| (raw.trim().parse() == Ok(channel)).then_some(key))
            .filter(|key| {
                candidate.herdr.endpoints.get(*key).is_some_and(|endpoint| {
                    endpoint.mode == session_hosts::EndpointMode::LegacyDedicated
                })
            })
            .ok_or("부팅 후보 설정이 이 채널을 legacy_dedicated endpoint로 매핑하지 않습니다.")?;
        let target = binding_digest(candidate, channel)
            .ok_or("부팅 후보 설정의 이 채널 매핑을 읽지 못했습니다.")?;
        let row = format!("channel {channel} row ");
        let refused = metadata.migration().is_some_and(|migration| {
            migration.reason == "session_hosts_migration_refused"
                && migration
                    .blockers
                    .contains(&format!("channel {channel} unresolved hold"))
                && !migration
                    .blockers
                    .iter()
                    .any(|blocker| blocker.starts_with(&row))
        });
        if !refused {
            return Err("이번 부팅의 migration은 이 채널의 보류 입력만으로 거부되지 않았습니다(live row 또는 다른 사유). 상태를 바꾸지 않았습니다.".into());
        }
        Ok((target, key.clone()))
    })
}

async fn reserve(data: &Data, channel: u64) -> Result<String, String> {
    let (target, key) = rollback_target(channel)?;
    let held = held(data, channel).await?;
    if held.is_empty() {
        return Err("반환할 보류 입력이 없습니다. 상태를 바꾸지 않았습니다.".into());
    }
    for (consent, intents) in &held {
        match consent.state {
            State::Denied | State::Expired | State::ApplyFailed => {}
            State::Pending => return Err("owner 결정이나 만료 뒤에 반환하세요.".into()),
            State::Accepted => return Err("설정을 적용하는 중입니다. 결과를 기다리세요.".into()),
            State::Applied => {
                return Err("이미 승인된 동의입니다. !herdr consent retry로 해제하세요.".into());
            }
        }
        let sink =
            custody::LiveSink::new(data.shared.clone(), data.provider.clone(), &consent.key)?;
        for intent in intents {
            if !sink.returnable(intent).await? {
                return Err(format!(
                    "입력 {}이 대기열에서 기다리는 상태가 아닙니다(전달 중이거나 복원 전). 상태를 바꾸지 않았습니다.",
                    intent.source_message
                ));
            }
        }
    }
    let mut count = 0;
    for (consent, intents) in held {
        count += intents.len();
        let returned = Resolution::Returned { target };
        settle(
            consent,
            intents.into_iter().map(|i| (i, returned)).collect(),
        )
        .await?;
    }
    Ok(format!(
        "입력 {count}건을 legacy 대상 `{key}`로 반환 예약했습니다. 재시작 뒤 기존 FIFO로 처리하며, 그 전에는 실행하지 않습니다."
    ))
}

/// Closes returns whose binding is the durable approval and whose custody no longer holds them.
async fn close(data: &Data, channel: u64) -> Result<String, String> {
    let approved = session_hosts::with_boot_migration(|metadata| {
        let durable = metadata.is_some_and(|metadata| {
            !metadata.admission_stopped()
                && metadata.migration().is_none_or(|migration| {
                    matches!(
                        migration.reason,
                        "session_hosts_migration_refused" | "session_hosts_migration_unverified"
                    )
                })
        });
        let approved = || session_hosts::with_boot(|boot| binding_digest(boot?.config(), channel));
        durable.then(approved).flatten()
    });
    let (mut closed, mut waiting, mut elsewhere) = (0, 0, 0);
    for (consent, intents) in held(data, channel).await? {
        let sink =
            custody::LiveSink::new(data.shared.clone(), data.provider.clone(), &consent.key)?;
        let mut changes = Vec::new();
        for intent in intents {
            let target = match consent.resolution(&intent) {
                Some(Resolution::Returned { target }) if approved == Some(target) => {
                    changes.push((intent.clone(), Resolution::Adopted { target }));
                    target
                }
                Some(Resolution::Adopted { target }) => target,
                Some(Resolution::Returned { .. }) => {
                    elsewhere += 1;
                    continue;
                }
                _ => continue,
            };
            // A read error or an unrestored live view is never evidence that the original left.
            if sink.mailbox_pending(&intent).await == Ok(false) {
                changes.push((intent, Resolution::Closed { target }));
                closed += 1;
            } else {
                waiting += 1;
            }
        }
        if !changes.is_empty() {
            settle(consent, changes).await?;
        }
    }
    if closed + waiting + elsewhere == 0 {
        return Err("이 채널에서 종료할 반환 입력이 없습니다. 상태를 바꾸지 않았습니다.".into());
    }
    Ok(format!(
        "반환 종료 {closed}건. 아직 처리되지 않은 반환 {waiting}건은 처리된 뒤 다시 실행하세요. 현재 승인 매핑과 다른 반환 {elsewhere}건은 그대로 둡니다."
    ))
}

#[cfg(test)]
#[path = "return_input_tests.rs"]
mod tests;
