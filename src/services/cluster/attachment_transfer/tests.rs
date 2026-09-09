use super::*;

fn identity() -> AttachmentMessageIdentity {
    AttachmentMessageIdentity {
        provider: "discord".to_string(),
        channel_id: "111222333".to_string(),
        user_msg_id: "444555666".to_string(),
    }
}

/// Build a well-formed entry: declared length and digest both match `bytes`.
fn entry(filename: &str, bytes: Vec<u8>) -> AttachmentEntryV1 {
    AttachmentEntryV1 {
        filename: filename.to_string(),
        byte_len: bytes.len() as u64,
        sha256: attachment_sha256_hex(&bytes),
        bytes,
    }
}

fn bundle(entries: Vec<AttachmentEntryV1>) -> AttachmentBundleV1 {
    AttachmentBundleV1 {
        version: ATTACHMENT_BUNDLE_V1,
        identity: identity(),
        entries,
    }
}

fn tiny_limits() -> AttachmentBundleLimits {
    AttachmentBundleLimits {
        max_entries: 2,
        max_entry_bytes: 4,
        max_total_bytes: 6,
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

    let validated = validate_attachment_bundle_v1(decoded, &identity(), &Default::default())
        .expect("round-tripped bundle validates");
    assert_eq!(validated.identity(), &identity());
    let names: Vec<&str> = validated
        .entries()
        .iter()
        .map(|e| e.filename.as_str())
        .collect();
    assert_eq!(names, ["photo.png", "payload.bin", "empty.bin"]);
    assert_eq!(validated.entries()[0].bytes, png);
    assert_eq!(validated.entries()[1].bytes, blob);
    assert!(validated.entries()[2].bytes.is_empty());

    // The shipped policy defaults are the #5713 S1 starting values.
    let defaults = AttachmentBundleLimits::default();
    assert_eq!(defaults.max_entries, 10);
    assert_eq!(defaults.max_entry_bytes, 8 * 1024 * 1024);
    assert_eq!(defaults.max_total_bytes, 16 * 1024 * 1024);
}

#[test]
fn bundle_rejects_cross_message_identity() {
    let limits = AttachmentBundleLimits::default();
    let good = bundle(vec![entry("photo.png", vec![1, 2, 3])]);
    validate_attachment_bundle_v1(good.clone(), &identity(), &limits)
        .expect("matching identity validates");

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
            validate_attachment_bundle_v1(good.clone(), &expected, &limits),
            Err(AttachmentBundleError::IdentityMismatch),
        );
    }
}

#[test]
fn bundle_rejects_corruption_and_limits() {
    let limits = tiny_limits();

    // Exactly at every ceiling: 2 entries, 4 bytes each way, 6 bytes total.
    validate_attachment_bundle_v1(
        bundle(vec![
            entry("a.bin", vec![1, 2, 3, 4]),
            entry("b.bin", vec![5, 6]),
        ]),
        &identity(),
        &limits,
    )
    .expect("a bundle sitting on every limit is accepted");

    let mut corrupted = bundle(vec![entry("a.bin", vec![1, 2, 3, 4])]);
    corrupted.entries[0].bytes[2] ^= 0x01;
    assert_eq!(
        validate_attachment_bundle_v1(corrupted, &identity(), &limits),
        Err(AttachmentBundleError::HashMismatch { index: 0 }),
    );

    let mut mislabelled = bundle(vec![entry("a.bin", vec![1, 2, 3])]);
    mislabelled.entries[0].byte_len = 4;
    assert_eq!(
        validate_attachment_bundle_v1(mislabelled, &identity(), &limits),
        Err(AttachmentBundleError::DeclaredLengthMismatch {
            index: 0,
            declared: 4,
            actual: 3,
        }),
    );

    assert_eq!(
        validate_attachment_bundle_v1(
            bundle(vec![
                entry("a.bin", vec![1]),
                entry("b.bin", vec![2]),
                entry("c.bin", vec![3]),
            ]),
            &identity(),
            &limits,
        ),
        Err(AttachmentBundleError::TooManyEntries { found: 3, max: 2 }),
    );

    assert_eq!(
        validate_attachment_bundle_v1(
            bundle(vec![entry("a.bin", vec![1, 2, 3, 4, 5])]),
            &identity(),
            &limits,
        ),
        Err(AttachmentBundleError::EntryTooLarge {
            index: 0,
            len: 5,
            max: 4,
        }),
    );

    // Both entries clear the per-entry ceiling; only the running total does not.
    assert_eq!(
        validate_attachment_bundle_v1(
            bundle(vec![
                entry("a.bin", vec![1, 2, 3, 4]),
                entry("b.bin", vec![5, 6, 7]),
            ]),
            &identity(),
            &limits,
        ),
        Err(AttachmentBundleError::TotalTooLarge { total: 7, max: 6 }),
    );
}

#[test]
fn bundle_rejects_unsafe_names_and_version() {
    let limits = AttachmentBundleLimits::default();

    // The receiver never joins these onto a directory, and the contract keeps
    // them out of the envelope so no consumer is tempted to.
    for name in [
        "../evil.png",
        "/etc/passwd",
        "dir/photo.png",
        "..\\evil.png",
        "photo\u{7}.png",
        "photo\n.png",
        "",
    ] {
        assert_eq!(
            validate_attachment_bundle_v1(
                bundle(vec![entry(name, vec![1, 2, 3])]),
                &identity(),
                &limits,
            ),
            Err(AttachmentBundleError::UnsafeFilename { index: 0 }),
            "filename {name:?} must be refused",
        );
    }

    for version in [0u16, 2, 9] {
        let mut wrong = bundle(vec![entry("photo.png", vec![1, 2, 3])]);
        wrong.version = version;
        assert_eq!(
            validate_attachment_bundle_v1(wrong, &identity(), &limits),
            Err(AttachmentBundleError::UnsupportedVersion { found: version }),
        );
    }

    // An attachment-carrying admission with no entries is a defect, never a
    // silent text-only turn.
    assert_eq!(
        validate_attachment_bundle_v1(bundle(Vec::new()), &identity(), &limits),
        Err(AttachmentBundleError::Empty),
    );
}
