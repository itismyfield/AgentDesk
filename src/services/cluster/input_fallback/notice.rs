//! Stable transition identity and message preparation, without an event producer or sender.

use poise::serenity_prelude::{CreateAllowedMentions, CreateMessage, Nonce};

use crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity;
use crate::services::message_outbox::dedupe_key_for_message;

pub(crate) struct Transition<'a> {
    pub identity: &'a OwnerIdentity,
    pub generation: i64,
    pub from: &'a str,
    pub to: &'a str,
    pub home: &'a str,
}

pub(crate) struct Labels<'a> {
    pub home: &'a str,
    pub to: &'a str,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Notice {
    pub target: String,
    pub session_key: String,
    pub reason_code: &'static str,
    pub nonce: String,
    pub dedupe_key: String,
    pub content: String,
}

fn fingerprint(parts: &[&str]) -> String {
    let mut digest = blake3::Hasher::new();
    for part in parts {
        digest.update(&(part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    digest.finalize().to_hex().to_string()
}

pub(crate) fn prepare(
    event: &Transition<'_>,
    labels: Labels<'_>,
    new_session: bool,
    home_changed: bool,
) -> Result<Option<Notice>, &'static str> {
    if event.identity.provider().is_empty()
        || event.identity.raw_channel_id().is_empty()
        || event.generation < 0
        || event.from.trim().is_empty()
        || event.to.trim().is_empty()
        || ![event.from, event.to].contains(&event.home)
    {
        return Err("invalid routing transition");
    }
    if event.from == event.to {
        return Ok(None);
    }
    let generation = event.generation.to_string();
    #[cfg(test)]
    let generation =
        if crate::services::cluster::intake_router_hook::owner_record::cleanup::tests::mutant(
            "notice-generation",
        ) {
            "0".into()
        } else {
            generation
        };
    let key = fingerprint(&[
        "routing-transition-v1",
        event.identity.provider(),
        event.identity.raw_channel_id(),
        &generation,
        event.from,
        event.to,
    ]);
    let returned = event.to == event.home;
    let reason_code = if returned {
        "routing.to_home"
    } else {
        "routing.to_peer"
    };
    let content = if returned && home_changed {
        format!(
            "home 설정을 적용해 이 채널의 입력을 {}에서 처리합니다.",
            labels.home
        )
    } else if returned {
        format!(
            "{} 연결이 돌아와 이 채널의 입력을 다시 {}에서 처리합니다.",
            labels.home, labels.home
        )
    } else {
        format!(
            "{}에서 입력을 받을 수 없어 {}에서 처리합니다.",
            labels.home, labels.to
        )
    };
    let content = if new_session {
        format!("{content} {}의 새 세션을 사용합니다.", labels.to)
    } else {
        content
    };
    let target = format!("channel:{}", event.identity.raw_channel_id());
    let session_key = format!("routing-transition:{key}");
    let nonce = fingerprint(&["routing-notice-nonce-v1", &key])[..24].to_owned();
    let dedupe_key =
        dedupe_key_for_message(&target, &content, Some(reason_code), Some(&session_key))
            .ok_or("missing outbox identity")?;
    Ok(Some(Notice {
        target,
        session_key,
        reason_code,
        nonce,
        dedupe_key,
        content,
    }))
}

impl Notice {
    pub(crate) fn discord_message(&self) -> CreateMessage {
        CreateMessage::new()
            .content(&self.content)
            .nonce(Nonce::String(self.nonce.clone()))
            .enforce_nonce(true)
            .allowed_mentions(
                CreateAllowedMentions::new()
                    .all_users(false)
                    .all_roles(false)
                    .everyone(false),
            )
    }
}

#[cfg(test)]
#[path = "notice/pg_tests.rs"]
mod pg_tests;
#[cfg(test)]
#[path = "notice/tests.rs"]
mod tests;
#[cfg(test)]
#[path = "notice/transport_tests.rs"]
mod transport_tests;
