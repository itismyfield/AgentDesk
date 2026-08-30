pub fn is_discord_snowflake(value: &str) -> bool {
    let value = value.trim();
    value.len() >= 15 && value.bytes().all(|byte| byte.is_ascii_digit())
}

pub fn normalize_discord_snowflake(value: Option<&str>) -> Option<&str> {
    value
        .map(str::trim)
        .filter(|value| is_discord_snowflake(value))
}

pub const MAX_DISCORD_RECIPIENT_IDS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscordRecipientIdListError {
    TooMany,
    InvalidOrDuplicate,
}

pub fn normalize_discord_recipient_ids(
    user_ids: &[String],
) -> Result<Vec<String>, DiscordRecipientIdListError> {
    if user_ids.len() > MAX_DISCORD_RECIPIENT_IDS {
        return Err(DiscordRecipientIdListError::TooMany);
    }
    let mut normalized = Vec::with_capacity(user_ids.len());
    for user_id in user_ids {
        let user_id = user_id.trim();
        let valid = !user_id.is_empty()
            && !user_id.starts_with('0')
            && user_id.bytes().all(|byte| byte.is_ascii_digit())
            && user_id.parse::<u64>().is_ok_and(|value| value > 0)
            && !normalized.iter().any(|existing| existing == user_id);
        if !valid {
            return Err(DiscordRecipientIdListError::InvalidOrDuplicate);
        }
        normalized.push(user_id.to_string());
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discord_snowflake_requires_long_numeric_id() {
        assert!(is_discord_snowflake("1490141479707086938"));
        assert!(is_discord_snowflake(" 1490141479707086938 "));
        assert!(!is_discord_snowflake("123"));
        assert!(!is_discord_snowflake("guild-123"));
        assert!(!is_discord_snowflake(""));
    }

    #[test]
    fn recipient_ids_are_trimmed_ordered_and_unique() {
        assert_eq!(
            normalize_discord_recipient_ids(&[
                " 1469509284508340276 ".to_string(),
                "1469509284508340277".to_string(),
            ])
            .unwrap(),
            vec![
                "1469509284508340276".to_string(),
                "1469509284508340277".to_string(),
            ]
        );
        assert_eq!(
            normalize_discord_recipient_ids(&[
                "1469509284508340276".to_string(),
                "1469509284508340276".to_string(),
            ]),
            Err(DiscordRecipientIdListError::InvalidOrDuplicate)
        );
    }

    #[test]
    fn recipient_ids_enforce_the_shared_limit() {
        let ids = (1..=MAX_DISCORD_RECIPIENT_IDS + 1)
            .map(|value| value.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            normalize_discord_recipient_ids(&ids),
            Err(DiscordRecipientIdListError::TooMany)
        );
    }
}
