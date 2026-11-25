use super::image_format::{
    MAX_IMAGE_BYTES, Rejected, check_bytes, encoded_fits, extension_matches,
};

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
