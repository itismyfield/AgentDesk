use super::image_format::{
    MAX_IMAGE_BYTES, Rejected, check_bytes, encoded_fits, extension_matches,
};
use super::{DiscordOutboundClient, HttpOutboundClient};
use crate::services::discord::krx_tui_o_test_host::repost_io::{Mock, reply};
use serde_json::{Value, json};

#[test]
fn image_format_keeps_scheduled_message_boundaries() {
    for (filename, mime, matches) in [
        ("a.PNG", "image/png", true),
        ("a.jpg", "image/jpeg", true),
        ("a.jpeg", "image/jpeg", true),
        ("a.webp", "image/webp", true),
        (".png", "image/png", false),
        ("a.png", "image/gif", false),
        ("png", "image/png", false),
    ] {
        assert_eq!(extension_matches(filename, mime), matches, "{filename}");
    }
    let edge = MAX_IMAGE_BYTES.div_ceil(3) * 4;
    assert!(encoded_fits(edge) && !encoded_fits(edge + 1));

    let mut max = vec![0; MAX_IMAGE_BYTES];
    max[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    assert_eq!(check_bytes("image/png", &max), Ok(()));
    max.push(0);
    assert_eq!(check_bytes("image/png", &max), Err(Rejected::Size));
    assert_eq!(check_bytes("image/png", b""), Err(Rejected::Size));
    for (mime, data, accepted) in [
        ("image/jpeg", &b"\xff\xd8\xff"[..], true),
        ("image/gif", &b"GIF87a"[..], true),
        ("image/webp", &b"RIFF\0\0\0\0WEBP"[..], true),
        ("image/webp", &b"RIFF\0\0\0\0WEBX"[..], false),
        ("image/png", &b"GIF89a"[..], false),
        ("image/bmp", &b"BM"[..], false),
    ] {
        let expected = if accepted {
            Ok(())
        } else {
            Err(Rejected::Magic)
        };
        assert_eq!(check_bytes(mime, data), expected, "{mime}");
    }
}

#[tokio::test]
async fn p2_allowed_mentions_default_http_client_wire_keeps_users() {
    let mock = Mock::start(vec![reply("200 OK", &[], r#"{"id":"p2-wire-message"}"#)]).await;
    let content = "<@1234> @everyone @here <@&5678> 기본 알림";
    let id = HttpOutboundClient::new(reqwest::Client::new(), "test-token", &mock.base)
        .post_message("123", content)
        .await
        .unwrap();
    assert_eq!(id, "p2-wire-message");
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "/channels/123/messages");
    let payload: Value = serde_json::from_slice(&requests[0].1).unwrap();
    assert_eq!(payload["content"], content);
    assert_eq!(payload["allowed_mentions"]["parse"], json!(["users"]));
}
