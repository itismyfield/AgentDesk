use crate::services;

pub fn handle_discord_sendfile(path: &str, channel_id: u64, hash_key: &str) {
    use crate::services::discord::resolve_discord_token_by_hash;
    let token = match resolve_discord_token_by_hash(hash_key) {
        Some(t) => t,
        None => {
            eprintln!(
                "Error: no Discord bot token found for hash key: {}",
                hash_key
            );
            std::process::exit(1);
        }
    };
    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
    rt.block_on(async {
        match services::discord::send_file_to_channel(&token, channel_id, path).await {
            Ok(_) => println!("File sent: {}", path),
            Err(e) => {
                eprintln!("Failed to send file: {}", e);
                std::process::exit(1);
            }
        }
    });
}

pub fn handle_discord_sendmessage(message: &str, channel_id: u64, hash_key: Option<&str>) {
    use crate::services::discord::{
        load_discord_bot_launch_configs, resolve_discord_token_by_hash,
    };

    let tokens: Vec<String> = match hash_key {
        Some(key) => match resolve_discord_token_by_hash(key) {
            Some(token) => vec![token],
            None => {
                eprintln!("Error: no Discord bot token found for hash key: {}", key);
                std::process::exit(1);
            }
        },
        None => load_discord_bot_launch_configs()
            .into_iter()
            .map(|cfg| cfg.token)
            .collect(),
    };

    if tokens.is_empty() {
        eprintln!(
            "Error: no configured Discord bot tokens found in agentdesk.yaml or credential files"
        );
        std::process::exit(1);
    }

    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
    rt.block_on(async {
        let mut last_error: Option<String> = None;
        for token in tokens {
            match services::discord::send_message_to_channel(&token, channel_id, message).await {
                Ok(_) => {
                    println!("Message sent to channel {}", channel_id);
                    return;
                }
                Err(err) => {
                    last_error = Some(err.to_string());
                }
            }
        }

        eprintln!(
            "Failed to send message: {}",
            last_error.unwrap_or_else(|| "unknown error".to_string())
        );
        std::process::exit(1);
    });
}

pub fn handle_discord_senddm(message: &str, user_id: u64, hash_key: Option<&str>) {
    use crate::services::discord::{
        load_discord_bot_launch_configs, resolve_discord_token_by_hash,
    };

    let tokens: Vec<String> = match hash_key {
        Some(key) => match resolve_discord_token_by_hash(key) {
            Some(token) => vec![token],
            None => {
                eprintln!("Error: no Discord bot token found for hash key: {}", key);
                std::process::exit(1);
            }
        },
        None => load_discord_bot_launch_configs()
            .into_iter()
            .map(|cfg| cfg.token)
            .collect(),
    };

    if tokens.is_empty() {
        eprintln!(
            "Error: no configured Discord bot tokens found in agentdesk.yaml or credential files"
        );
        std::process::exit(1);
    }

    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
    rt.block_on(async {
        let mut last_error: Option<String> = None;
        for token in tokens {
            match services::discord::send_message_to_user(&token, user_id, message).await {
                Ok(_) => {
                    println!("Message sent to user {}", user_id);
                    return;
                }
                Err(err) => {
                    last_error = Some(err.to_string());
                }
            }
        }

        eprintln!(
            "Failed to send DM: {}",
            last_error.unwrap_or_else(|| "unknown error".to_string())
        );
        std::process::exit(1);
    });
}
