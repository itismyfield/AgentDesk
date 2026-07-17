use crate::services;

#[derive(Clone, Copy)]
enum LegacyDiscordPayload<'a> {
    File(&'a str),
    Message(&'a str),
}

#[derive(Clone, Copy)]
enum LegacyDiscordDestination {
    Channel(u64),
    User(u64),
}

trait LegacyDiscordSender {
    async fn send_file(
        &self,
        token: &str,
        channel_id: u64,
        path: &str,
    ) -> Result<(), Box<dyn std::error::Error>>;

    async fn send_channel_message(
        &self,
        token: &str,
        channel_id: u64,
        message: &str,
    ) -> Result<(), Box<dyn std::error::Error>>;

    async fn send_user_message(
        &self,
        token: &str,
        user_id: u64,
        message: &str,
    ) -> Result<(), Box<dyn std::error::Error>>;
}

struct ServiceLegacyDiscordSender;

impl LegacyDiscordSender for ServiceLegacyDiscordSender {
    async fn send_file(
        &self,
        token: &str,
        channel_id: u64,
        path: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        services::discord::send_file_to_channel(token, channel_id, path).await
    }

    async fn send_channel_message(
        &self,
        token: &str,
        channel_id: u64,
        message: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        services::discord::send_message_to_channel(token, channel_id, message).await
    }

    async fn send_user_message(
        &self,
        token: &str,
        user_id: u64,
        message: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        services::discord::send_message_to_user(token, user_id, message).await
    }
}

impl LegacyDiscordPayload<'_> {
    async fn send<S: LegacyDiscordSender + ?Sized>(
        self,
        sender: &S,
        token: &str,
        destination: LegacyDiscordDestination,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match (self, destination) {
            (Self::File(path), LegacyDiscordDestination::Channel(channel_id)) => {
                sender.send_file(token, channel_id, path).await
            }
            (Self::Message(message), LegacyDiscordDestination::Channel(channel_id)) => {
                sender
                    .send_channel_message(token, channel_id, message)
                    .await
            }
            (Self::Message(message), LegacyDiscordDestination::User(user_id)) => {
                sender.send_user_message(token, user_id, message).await
            }
            (Self::File(_), LegacyDiscordDestination::User(_)) => {
                unreachable!("legacy Discord file sends only support channels")
            }
        }
    }
}

impl LegacyDiscordDestination {
    fn success_message(self, payload: LegacyDiscordPayload<'_>) -> String {
        match (payload, self) {
            (LegacyDiscordPayload::File(path), Self::Channel(_)) => format!("File sent: {path}"),
            (LegacyDiscordPayload::Message(_), Self::Channel(channel_id)) => {
                format!("Message sent to channel {channel_id}")
            }
            (LegacyDiscordPayload::Message(_), Self::User(user_id)) => {
                format!("Message sent to user {user_id}")
            }
            (LegacyDiscordPayload::File(_), Self::User(_)) => {
                unreachable!("legacy Discord file sends only support channels")
            }
        }
    }

    fn failure_message(self, payload: LegacyDiscordPayload<'_>, error: &str) -> String {
        match (payload, self) {
            (LegacyDiscordPayload::File(_), Self::Channel(_)) => {
                format!("Failed to send file: {error}")
            }
            (LegacyDiscordPayload::Message(_), Self::Channel(_)) => {
                format!("Failed to send message: {error}")
            }
            (LegacyDiscordPayload::Message(_), Self::User(_)) => {
                format!("Failed to send DM: {error}")
            }
            (LegacyDiscordPayload::File(_), Self::User(_)) => {
                unreachable!("legacy Discord file sends only support channels")
            }
        }
    }

    fn supports(self, payload: LegacyDiscordPayload<'_>) -> bool {
        !matches!(
            (payload, self),
            (LegacyDiscordPayload::File(_), Self::User(_))
        )
    }
}

#[derive(Clone, Copy)]
enum LegacyDiscordTokenSelection<'a> {
    RequiredHash(&'a str),
    OptionalHash(Option<&'a str>),
}

struct LegacyDiscordSend<'a> {
    payload: LegacyDiscordPayload<'a>,
    destination: LegacyDiscordDestination,
    token_selection: LegacyDiscordTokenSelection<'a>,
}

fn resolve_legacy_discord_tokens(
    selection: LegacyDiscordTokenSelection<'_>,
) -> Result<Vec<String>, String> {
    resolve_legacy_discord_tokens_with(
        selection,
        crate::services::discord::resolve_discord_token_by_hash,
        || {
            crate::services::discord::load_discord_bot_launch_configs()
                .into_iter()
                .map(|config| config.token)
                .collect()
        },
    )
}

fn resolve_legacy_discord_tokens_with<R, L>(
    selection: LegacyDiscordTokenSelection<'_>,
    resolve_hash: R,
    load_configured: L,
) -> Result<Vec<String>, String>
where
    R: FnOnce(&str) -> Option<String>,
    L: FnOnce() -> Vec<String>,
{
    let tokens = match selection {
        LegacyDiscordTokenSelection::RequiredHash(key)
        | LegacyDiscordTokenSelection::OptionalHash(Some(key)) => resolve_hash(key)
            .map(|token| vec![token])
            .ok_or_else(|| format!("Error: no Discord bot token found for hash key: {key}"))?,
        LegacyDiscordTokenSelection::OptionalHash(None) => load_configured(),
    };

    if tokens.is_empty() {
        Err(
            "Error: no configured Discord bot tokens found in agentdesk.yaml or credential files"
                .to_string(),
        )
    } else {
        Ok(tokens)
    }
}

fn execute_legacy_discord_send(request: LegacyDiscordSend<'_>) {
    debug_assert!(request.destination.supports(request.payload));
    let tokens = match resolve_legacy_discord_tokens(request.token_selection) {
        Ok(tokens) => tokens,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };

    let runtime = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
    runtime.block_on(async move {
        let mut last_error = None;
        for token in tokens {
            match request
                .payload
                .send(&ServiceLegacyDiscordSender, &token, request.destination)
                .await
            {
                Ok(()) => {
                    println!("{}", request.destination.success_message(request.payload));
                    return;
                }
                Err(error) => last_error = Some(error.to_string()),
            }
        }

        let error = last_error.unwrap_or_else(|| "unknown error".to_string());
        eprintln!(
            "{}",
            request.destination.failure_message(request.payload, &error)
        );
        std::process::exit(1);
    });
}

pub fn handle_discord_sendfile(path: &str, channel_id: u64, hash_key: &str) {
    execute_legacy_discord_send(LegacyDiscordSend {
        payload: LegacyDiscordPayload::File(path),
        destination: LegacyDiscordDestination::Channel(channel_id),
        token_selection: LegacyDiscordTokenSelection::RequiredHash(hash_key),
    });
}

pub fn handle_discord_sendmessage(message: &str, channel_id: u64, hash_key: Option<&str>) {
    execute_legacy_discord_send(LegacyDiscordSend {
        payload: LegacyDiscordPayload::Message(message),
        destination: LegacyDiscordDestination::Channel(channel_id),
        token_selection: LegacyDiscordTokenSelection::OptionalHash(hash_key),
    });
}

pub fn handle_discord_senddm(message: &str, user_id: u64, hash_key: Option<&str>) {
    execute_legacy_discord_send(LegacyDiscordSend {
        payload: LegacyDiscordPayload::Message(message),
        destination: LegacyDiscordDestination::User(user_id),
        token_selection: LegacyDiscordTokenSelection::OptionalHash(hash_key),
    });
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::{
        LegacyDiscordDestination, LegacyDiscordPayload, LegacyDiscordSender,
        LegacyDiscordTokenSelection, resolve_legacy_discord_tokens_with,
    };

    #[derive(Debug, PartialEq, Eq)]
    enum RecordedSend {
        File {
            token: String,
            channel_id: u64,
            path: String,
        },
        ChannelMessage {
            token: String,
            channel_id: u64,
            message: String,
        },
        UserMessage {
            token: String,
            user_id: u64,
            message: String,
        },
    }

    #[derive(Default)]
    struct RecordingSender {
        sends: RefCell<Vec<RecordedSend>>,
    }

    impl LegacyDiscordSender for RecordingSender {
        async fn send_file(
            &self,
            token: &str,
            channel_id: u64,
            path: &str,
        ) -> Result<(), Box<dyn std::error::Error>> {
            self.sends.borrow_mut().push(RecordedSend::File {
                token: token.to_string(),
                channel_id,
                path: path.to_string(),
            });
            Ok(())
        }

        async fn send_channel_message(
            &self,
            token: &str,
            channel_id: u64,
            message: &str,
        ) -> Result<(), Box<dyn std::error::Error>> {
            self.sends.borrow_mut().push(RecordedSend::ChannelMessage {
                token: token.to_string(),
                channel_id,
                message: message.to_string(),
            });
            Ok(())
        }

        async fn send_user_message(
            &self,
            token: &str,
            user_id: u64,
            message: &str,
        ) -> Result<(), Box<dyn std::error::Error>> {
            self.sends.borrow_mut().push(RecordedSend::UserMessage {
                token: token.to_string(),
                user_id,
                message: message.to_string(),
            });
            Ok(())
        }
    }

    #[test]
    fn hashed_token_selection_is_shared_by_all_legacy_send_shapes() {
        for selection in [
            LegacyDiscordTokenSelection::RequiredHash("file-key"),
            LegacyDiscordTokenSelection::OptionalHash(Some("message-key")),
            LegacyDiscordTokenSelection::OptionalHash(Some("dm-key")),
        ] {
            let configured_loaded = Cell::new(false);
            let expected_key = match selection {
                LegacyDiscordTokenSelection::RequiredHash(key)
                | LegacyDiscordTokenSelection::OptionalHash(Some(key)) => key,
                LegacyDiscordTokenSelection::OptionalHash(None) => unreachable!(),
            };
            let tokens = resolve_legacy_discord_tokens_with(
                selection,
                |key| Some(format!("token-for-{key}")),
                || {
                    configured_loaded.set(true);
                    vec!["configured-token".to_string()]
                },
            )
            .expect("hash should resolve");

            assert_eq!(tokens, vec![format!("token-for-{expected_key}")]);
            assert!(!configured_loaded.get());
        }
    }

    #[test]
    fn hash_resolution_failure_keeps_the_legacy_error_text() {
        let error = resolve_legacy_discord_tokens_with(
            LegacyDiscordTokenSelection::OptionalHash(Some("missing-key")),
            |_| None,
            Vec::new,
        )
        .unwrap_err();

        assert_eq!(
            error,
            "Error: no Discord bot token found for hash key: missing-key"
        );
    }

    #[test]
    fn configured_token_fallback_keeps_order_and_empty_error() {
        let tokens = resolve_legacy_discord_tokens_with(
            LegacyDiscordTokenSelection::OptionalHash(None),
            |_| panic!("hash resolver must not run"),
            || vec!["first".to_string(), "second".to_string()],
        )
        .expect("configured tokens should resolve");
        assert_eq!(tokens, vec!["first".to_string(), "second".to_string()]);

        let error = resolve_legacy_discord_tokens_with(
            LegacyDiscordTokenSelection::OptionalHash(None),
            |_| panic!("hash resolver must not run"),
            Vec::new,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "Error: no configured Discord bot tokens found in agentdesk.yaml or credential files"
        );
    }

    #[test]
    fn payload_messages_preserve_each_legacy_cli_contract() {
        let file = LegacyDiscordPayload::File("/tmp/report.txt");
        let channel = LegacyDiscordDestination::Channel(42);
        assert_eq!(channel.success_message(file), "File sent: /tmp/report.txt");
        assert_eq!(
            channel.failure_message(file, "denied"),
            "Failed to send file: denied"
        );

        let message = LegacyDiscordPayload::Message("hello");
        assert_eq!(
            channel.success_message(message),
            "Message sent to channel 42"
        );
        assert_eq!(
            channel.failure_message(message, "denied"),
            "Failed to send message: denied"
        );

        let user = LegacyDiscordDestination::User(43);
        assert_eq!(user.success_message(message), "Message sent to user 43");
        assert_eq!(
            user.failure_message(message, "denied"),
            "Failed to send DM: denied"
        );
    }

    #[test]
    fn destination_contract_accepts_only_the_three_legacy_send_shapes() {
        let channel = LegacyDiscordDestination::Channel(42);
        let user = LegacyDiscordDestination::User(43);
        assert!(channel.supports(LegacyDiscordPayload::File("report.txt")));
        assert!(channel.supports(LegacyDiscordPayload::Message("hello")));
        assert!(user.supports(LegacyDiscordPayload::Message("hello")));
        assert!(!user.supports(LegacyDiscordPayload::File("report.txt")));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn payload_dispatches_to_the_matching_low_level_sender() {
        let sender = RecordingSender::default();

        LegacyDiscordPayload::File("/tmp/report.txt")
            .send(&sender, "file-token", LegacyDiscordDestination::Channel(42))
            .await
            .expect("file dispatch should succeed");
        LegacyDiscordPayload::Message("channel body")
            .send(
                &sender,
                "channel-token",
                LegacyDiscordDestination::Channel(43),
            )
            .await
            .expect("channel message dispatch should succeed");
        LegacyDiscordPayload::Message("dm body")
            .send(&sender, "dm-token", LegacyDiscordDestination::User(44))
            .await
            .expect("user message dispatch should succeed");

        assert_eq!(
            *sender.sends.borrow(),
            vec![
                RecordedSend::File {
                    token: "file-token".to_string(),
                    channel_id: 42,
                    path: "/tmp/report.txt".to_string(),
                },
                RecordedSend::ChannelMessage {
                    token: "channel-token".to_string(),
                    channel_id: 43,
                    message: "channel body".to_string(),
                },
                RecordedSend::UserMessage {
                    token: "dm-token".to_string(),
                    user_id: 44,
                    message: "dm body".to_string(),
                },
            ]
        );
    }
}
