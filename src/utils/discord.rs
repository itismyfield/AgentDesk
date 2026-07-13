use serenity::model::channel::ChannelType;

pub fn is_thread_channel_type(kind: ChannelType) -> bool {
    matches!(
        kind,
        ChannelType::NewsThread | ChannelType::PublicThread | ChannelType::PrivateThread
    )
}

pub fn is_discord_snowflake(value: &str) -> bool {
    let value = value.trim();
    value.len() >= 15 && value.bytes().all(|byte| byte.is_ascii_digit())
}

pub fn normalize_discord_snowflake(value: Option<&str>) -> Option<&str> {
    value
        .map(str::trim)
        .filter(|value| is_discord_snowflake(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_channel_type_recognizes_all_discord_thread_kinds() {
        for kind in [
            ChannelType::NewsThread,
            ChannelType::PublicThread,
            ChannelType::PrivateThread,
        ] {
            assert!(
                is_thread_channel_type(kind),
                "expected {kind:?} to be a thread"
            );
        }
    }

    #[test]
    fn thread_channel_type_rejects_non_thread_kinds() {
        for kind in [ChannelType::News, ChannelType::Text, ChannelType::Forum] {
            assert!(
                !is_thread_channel_type(kind),
                "expected {kind:?} not to be a thread"
            );
        }
    }

    #[test]
    fn discord_snowflake_requires_long_numeric_id() {
        assert!(is_discord_snowflake("1490141479707086938"));
        assert!(is_discord_snowflake(" 1490141479707086938 "));
        assert!(!is_discord_snowflake("123"));
        assert!(!is_discord_snowflake("guild-123"));
        assert!(!is_discord_snowflake(""));
    }
}
