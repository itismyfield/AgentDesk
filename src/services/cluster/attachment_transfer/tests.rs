use super::*;

fn identity() -> AttachmentMessageIdentity {
    AttachmentMessageIdentity {
        provider: "discord".to_string(),
        channel_id: "111222333".to_string(),
        user_msg_id: "444555666".to_string(),
    }
}

/// A well-formed entry: the digest matches `bytes`.
fn entry(filename: &str, bytes: Vec<u8>) -> AttachmentEntryV1 {
    AttachmentEntryV1 {
        filename: filename.to_string(),
        sha256: attachment_sha256_hex(&bytes),
        bytes,
    }
}

/// A bundle whose declared source count matches the entries it carries.
fn bundle(entries: Vec<AttachmentEntryV1>) -> AttachmentBundleV1 {
    AttachmentBundleV1 {
        version: ATTACHMENT_BUNDLE_V1,
        identity: identity(),
        source_attachment_count: entries.len() as u32,
        entries,
    }
}

#[test]
fn bundle_roundtrip_preserves_order_and_binary() {
    // A PNG header with embedded NULs, an all-byte-values blob, and a 0-byte
    // file: the three shapes a naive text-oriented envelope would corrupt.
    let png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x00, 0x1a, 0x0a, 0x00];
    let blob: Vec<u8> = (0..=u8::MAX).collect();
    let entries = vec![
        entry("photo.png", png.clone()),
        entry("payload.bin", blob.clone()),
        entry("empty.bin", Vec::new()),
    ];

    let encoded = serde_json::to_string(&bundle(entries.clone())).expect("serialize bundle");
    let decoded: AttachmentBundleV1 = serde_json::from_str(&encoded).expect("deserialize bundle");
    assert_eq!(decoded, bundle(entries));

    let validated = validate_attachment_bundle_v1(decoded, &identity())
        .expect("round-tripped bundle validates");
    let got = validated.as_bundle();
    assert_eq!(got.identity, identity());
    assert_eq!(got.source_attachment_count, 3);
    let names: Vec<&str> = got.entries.iter().map(|e| e.filename.as_str()).collect();
    assert_eq!(names, ["photo.png", "payload.bin", "empty.bin"]);
    assert_eq!(got.entries[0].bytes, png);
    assert_eq!(got.entries[1].bytes, blob);
    assert!(got.entries[2].bytes.is_empty());
}

#[test]
fn bundle_rejects_cross_message_identity() {
    let good = bundle(vec![entry("photo.png", vec![1, 2, 3])]);
    validate_attachment_bundle_v1(good.clone(), &identity()).expect("matching identity validates");

    // One differing field is enough — otherwise a bundle prepared for one bot
    // or channel could be consumed by another's message.
    for expected in [
        AttachmentMessageIdentity {
            provider: "slack".to_string(),
            ..identity()
        },
        AttachmentMessageIdentity {
            channel_id: "999888777".to_string(),
            ..identity()
        },
        AttachmentMessageIdentity {
            user_msg_id: "123123123".to_string(),
            ..identity()
        },
    ] {
        assert_eq!(
            validate_attachment_bundle_v1(good.clone(), &expected),
            Err(AttachmentBundleError::IdentityMismatch),
        );
    }
}

#[test]
fn bundle_rejects_partial_entry_set() {
    // The producer downloaded 2 of the message's 3 attachments. Version,
    // identity, and both digests are intact, so without the declared count this
    // bundle validates and the agent silently receives a shorter turn.
    let mut partial = bundle(vec![entry("a.png", vec![1]), entry("b.png", vec![2])]);
    partial.source_attachment_count = 3;
    assert_eq!(
        validate_attachment_bundle_v1(partial, &identity()),
        Err(AttachmentBundleError::IncompleteBundle),
    );

    // A padded set is refused on the same footing as a short one.
    let mut padded = bundle(vec![entry("a.png", vec![1]), entry("b.png", vec![2])]);
    padded.source_attachment_count = 1;
    assert_eq!(
        validate_attachment_bundle_v1(padded, &identity()),
        Err(AttachmentBundleError::IncompleteBundle),
    );

    // An attachment-carrying admission with no entries is a defect, never a
    // silent text-only turn.
    assert_eq!(
        validate_attachment_bundle_v1(bundle(Vec::new()), &identity()),
        Err(AttachmentBundleError::IncompleteBundle),
    );
}

#[test]
fn bundle_rejects_corrupted_bytes_and_unknown_version() {
    // A single flipped bit keeps every length and count intact, so only the
    // digest can catch it.
    let mut corrupted = bundle(vec![entry("a.bin", vec![1, 2, 3, 4])]);
    corrupted.entries[0].bytes[2] ^= 0x01;
    assert_eq!(
        validate_attachment_bundle_v1(corrupted, &identity()),
        Err(AttachmentBundleError::HashMismatch),
    );

    for version in [0u16, 2, 9] {
        let mut wrong = bundle(vec![entry("photo.png", vec![1, 2, 3])]);
        wrong.version = version;
        assert_eq!(
            validate_attachment_bundle_v1(wrong, &identity()),
            Err(AttachmentBundleError::UnsupportedVersion),
        );
    }
}
