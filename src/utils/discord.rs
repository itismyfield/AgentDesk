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
    fn discord_snowflake_requires_long_numeric_id() {
        assert!(is_discord_snowflake("1490141479707086938"));
        assert!(is_discord_snowflake(" 1490141479707086938 "));
        assert!(!is_discord_snowflake("123"));
        assert!(!is_discord_snowflake("guild-123"));
        assert!(!is_discord_snowflake(""));
    }
}
