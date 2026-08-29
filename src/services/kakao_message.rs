//! Provider-neutral validation and Kakao default-template construction.
//!
//! Kakao fetches linked images itself. URLs are therefore restricted to
//! bounded public HTTPS locations before they leave AgentDesk.

use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct KakaoMessage {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum KakaoMessageValidationError {
    #[error("text must contain 1 to 200 characters")]
    InvalidText,
    #[error("{0} must be a public HTTPS URL")]
    InvalidPublicUrl(&'static str),
}

pub fn validate_message(message: &KakaoMessage) -> Result<(), KakaoMessageValidationError> {
    let char_count = message.text.chars().count();
    if message.text.trim().is_empty() || char_count > 200 {
        return Err(KakaoMessageValidationError::InvalidText);
    }
    if let Some(image_url) = message.image_url.as_deref() {
        validate_public_https_url(image_url, "image_url")?;
    }
    Ok(())
}

pub fn validate_public_https_url(
    raw: &str,
    field: &'static str,
) -> Result<(), KakaoMessageValidationError> {
    if raw.is_empty() || raw.len() > 2_048 || raw.trim() != raw {
        return Err(KakaoMessageValidationError::InvalidPublicUrl(field));
    }
    let url = reqwest::Url::parse(raw)
        .map_err(|_| KakaoMessageValidationError::InvalidPublicUrl(field))?;
    let host_allowed = match url.host() {
        Some(url::Host::Domain(host)) => {
            !host.eq_ignore_ascii_case("localhost") && !is_private_ip_literal(host)
        }
        Some(url::Host::Ipv4(address)) => !is_blocked_ipv4(address),
        Some(url::Host::Ipv6(address)) => !is_blocked_ipv6(address),
        None => false,
    };
    if url.scheme() != "https"
        || url.username() != ""
        || url.password().is_some()
        || url.port().is_some()
        || !host_allowed
    {
        return Err(KakaoMessageValidationError::InvalidPublicUrl(field));
    }
    Ok(())
}

pub(crate) fn default_template(message: &KakaoMessage, landing_url: &str) -> String {
    let link = json!({
        "web_url": landing_url,
        "mobile_web_url": landing_url
    });
    match message.image_url.as_deref() {
        Some(image_url) => json!({
            "object_type": "feed",
            "content": {
                "title": feed_title(&message.text),
                "description": message.text,
                "image_url": image_url,
                "link": link
            },
            "button_title": "열기"
        })
        .to_string(),
        None => json!({
            "object_type": "text",
            "text": message.text,
            "link": link
        })
        .to_string(),
    }
}

fn feed_title(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("예약 메시지")
        .chars()
        .take(50)
        .collect()
}

fn is_private_ip_literal(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let Ok(address) = host.parse::<std::net::IpAddr>() else {
        return false;
    };
    match address {
        std::net::IpAddr::V4(address) => is_blocked_ipv4(address),
        std::net::IpAddr::V6(address) => is_blocked_ipv6(address),
    }
}

fn is_blocked_ipv6(address: std::net::Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_blocked_ipv4(mapped);
    }
    let segments = address.segments();
    address.is_loopback()
        || address.is_unspecified()
        || address.is_unique_local()
        || address.is_unicast_link_local()
        || address.is_multicast()
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
}

fn is_blocked_ipv4(address: std::net::Ipv4Addr) -> bool {
    let octets = address.octets();
    address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_multicast()
        || address.is_unspecified()
        || address.is_broadcast()
        || matches!(
            octets,
            [0, _, _, _]
                | [100, 64..=127, _, _]
                | [192, 0, 2, _]
                | [198, 18..=19, _, _]
                | [198, 51, 100, _]
                | [203, 0, 113, _]
                | [240..=255, _, _, _]
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_message(text: &str) -> KakaoMessage {
        KakaoMessage {
            text: text.to_string(),
            image_url: None,
        }
    }

    #[test]
    fn text_template_contains_only_the_configured_public_link() {
        let value: serde_json::Value = serde_json::from_str(&default_template(
            &text_message("예약 알림"),
            "https://example.com/messages",
        ))
        .unwrap();
        assert_eq!(value["object_type"], "text");
        assert_eq!(value["text"], "예약 알림");
        assert_eq!(value["link"]["web_url"], "https://example.com/messages");
    }

    #[test]
    fn feed_template_uses_a_bounded_first_line_title() {
        let mut message = text_message(&format!("{}\n본문", "가".repeat(80)));
        message.image_url = Some("https://cdn.example.com/image.jpg".to_string());
        let value: serde_json::Value =
            serde_json::from_str(&default_template(&message, "https://example.com/messages"))
                .unwrap();
        assert_eq!(value["object_type"], "feed");
        assert_eq!(
            value["content"]["title"].as_str().unwrap().chars().count(),
            50
        );
    }

    #[test]
    fn validation_rejects_private_or_credentialed_urls() {
        for invalid in [
            "http://example.com/image.jpg",
            "https://127.0.0.1/image.jpg",
            "https://[::1]/image.jpg",
            "https://[::ffff:127.0.0.1]/image.jpg",
            "https://user@example.com/image.jpg",
            "https://example.com:8443/image.jpg",
        ] {
            assert!(validate_public_https_url(invalid, "image_url").is_err());
        }
        assert!(
            validate_public_https_url("https://cdn.example.com/image.jpg", "image_url").is_ok()
        );
    }

    #[test]
    fn message_text_is_trimmed_for_validation_but_not_rewritten() {
        assert!(validate_message(&text_message("   ")).is_err());
        assert!(validate_message(&text_message(&"a".repeat(201))).is_err());
        assert!(validate_message(&text_message("  keep spacing  ")).is_ok());
    }
}
