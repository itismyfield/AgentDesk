use crate::services::discord::SharedData;
use crate::services::discord::http::{
    edit_channel_message_with_components, send_channel_message_with_components,
};
use crate::services::provider::ProviderKind;
use crate::services::session_host::policy::{
    PeerSource,
    consent::{
        Consent,
        card::Card,
        engine::{Cards, Owner, SendFailure},
        preview,
        workflow::Current,
    },
};
use poise::serenity_prelude as serenity;
use std::sync::Arc;

pub(super) struct LiveCards {
    pub http: Arc<serenity::Http>,
    pub channel: serenity::ChannelId,
}
fn components(card: &Card) -> Vec<serenity::CreateActionRow> {
    if card.buttons.is_empty() {
        return Vec::new();
    }
    vec![serenity::CreateActionRow::Buttons(
        card.buttons
            .iter()
            .map(|button| {
                serenity::CreateButton::new(&button.custom_id)
                    .label(button.label)
                    .style(serenity::ButtonStyle::Primary)
                    .disabled(button.disabled)
            })
            .collect(),
    )]
}
fn send_failure(error: serenity::Error) -> SendFailure {
    if matches!(&error, serenity::Error::Http(http) if http.status_code().is_some_and(|status| matches!(status.as_u16(), 400 | 401 | 403 | 404)))
    {
        SendFailure::Definitive(error.to_string())
    } else {
        SendFailure::Unknown(error.to_string())
    }
}
#[async_trait::async_trait]
impl Cards for LiveCards {
    async fn send(&mut self, card: &Card) -> Result<u64, SendFailure> {
        send_channel_message_with_components(
            &self.http,
            self.channel,
            &card.content,
            components(card),
        )
        .await
        .map(|message| message.id.get())
        .map_err(send_failure)
    }
    async fn replace(&mut self, message: u64, card: &Card) -> Result<(), String> {
        edit_channel_message_with_components(
            &self.http,
            self.channel,
            serenity::MessageId::new(message),
            &card.content,
            components(card),
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }
    async fn disable(&mut self, message: u64) -> Result<(), String> {
        // Preserve the existing guidance while removing its interactive buttons.
        let current = self
            .channel
            .message(&self.http, message)
            .await
            .map_err(|error| error.to_string())?;
        edit_channel_message_with_components(
            &self.http,
            self.channel,
            serenity::MessageId::new(message),
            &current.content,
            Vec::new(),
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;

pub(super) struct LiveOwner {
    pub http: Arc<serenity::Http>,
    pub shared: Arc<SharedData>,
    pub provider: ProviderKind,
    pub source: Arc<dyn PeerSource>,
}
#[async_trait::async_trait]
impl Owner for LiveOwner {
    async fn current(&mut self, consent: &Consent) -> Result<Current, String> {
        let installed =
            super::endpoint(consent.channel).ok_or("consent endpoint is no longer user-default")?;
        if installed.execution_node != consent.key.node
            || installed.socket_addr.as_str() != consent.key.socket.to_string_lossy().as_ref()
            || installed.herdr_session != consent.key.named_session
            || consent.provider != self.provider.as_str()
        {
            return Err("consent endpoint/provider changed; preserve held inputs".into());
        }
        let settings = self.shared.settings.read().await.clone();
        if settings.owner_user_id != Some(consent.owner) {
            return Err("consent owner changed; preserve held inputs".into());
        }
        crate::services::discord::session_runtime::validate_rest_channel_routing(
            &self.http,
            None,
            &self.provider,
            &settings,
            serenity::ChannelId::new(consent.channel),
            None,
        )
        .await
        .map_err(|_| "consent provider/channel binding changed".to_string())?;
        let bot = self
            .http
            .get_current_user()
            .await
            .map_err(|error| error.to_string())?
            .id
            .get();
        let prepared = preview::prepare(self.source.clone(), consent.preview.revision).await?;
        if !prepared.matches_key(&consent.key) {
            return Err("consent logical/target path changed; preserve held inputs".into());
        }
        Ok(Current {
            owner: settings.owner_user_id,
            bot,
            provider: self.provider.as_str().into(),
            channel: consent.channel,
            message: consent.message,
            nonce: consent.nonce.clone(),
            key: consent.key.clone(),
            preview: prepared.token,
            local_node: crate::config::session_hosts::local_node().unwrap_or_default(),
        })
    }
}
