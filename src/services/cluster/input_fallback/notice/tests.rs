use super::*;

fn event<'a>(
    identity: &'a OwnerIdentity,
    generation: i64,
    from: &'a str,
    to: &'a str,
) -> Transition<'a> {
    Transition {
        identity,
        generation,
        from,
        to,
        home: "A",
    }
}

fn rendered(event: &Transition<'_>) -> Notice {
    prepare(
        event,
        Labels {
            home: "개인 노트북",
            to: "책상 컴퓨터",
        },
        false,
        false,
    )
    .unwrap()
    .unwrap()
}

#[test]
fn routing_notice_keys_separate_generations_and_reuse_exact_event() {
    let identity = OwnerIdentity::new(" Claude ", " 7001 ");
    let first = rendered(&event(&identity, 1, "A", "B"));
    let returned = rendered(&event(&identity, 2, "B", "A"));
    let again = rendered(&event(&identity, 3, "A", "B"));
    assert_eq!(first, rendered(&event(&identity, 1, "A", "B")));
    assert_eq!(first.content, again.content);
    for (a, b) in [(&first, &returned), (&returned, &again), (&first, &again)] {
        assert_ne!(a.session_key, b.session_key);
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.dedupe_key, b.dedupe_key);
    }
    assert_eq!(first.target, "channel:7001");
    assert!(first.nonce.len() <= 25);
    let other_scope = OwnerIdentity::new("codex", "7001");
    assert_ne!(
        first.dedupe_key,
        rendered(&event(&other_scope, 1, "A", "B")).dedupe_key
    );
    assert_ne!(fingerprint(&["a", "bc"]), fingerprint(&["ab", "c"]));
}

#[test]
fn routing_notice_renders_actual_devices_and_session_choice() {
    let identity = OwnerIdentity::new("codex", "7001");
    let fallback = event(&identity, 1, "A", "B");
    let existing = rendered(&fallback);
    assert_eq!(
        existing.content,
        "개인 노트북에서 입력을 받을 수 없어 책상 컴퓨터에서 처리합니다."
    );
    assert_eq!(existing.reason_code, "routing.to_peer");
    let fresh = prepare(
        &fallback,
        Labels {
            home: "작업실",
            to: "휴대 장치",
        },
        true,
        false,
    )
    .unwrap()
    .unwrap();
    assert!(fresh.content.contains("휴대 장치의 새 세션"));
    assert!(!existing.content.contains("새 세션"));
    let returning = event(&identity, 2, "B", "A");
    assert_eq!(rendered(&returning).reason_code, "routing.to_home");
    let changed = prepare(
        &returning,
        Labels {
            home: "작업실",
            to: "작업실",
        },
        false,
        true,
    )
    .unwrap()
    .unwrap();
    assert!(changed.content.contains("home 설정을 적용해"));
    assert!(!changed.content.contains("연결이 돌아와"));
    let reversed = Transition {
        home: "B",
        ..fallback
    };
    let reverse = prepare(
        &reversed,
        Labels {
            home: "책상 컴퓨터",
            to: "개인 노트북",
        },
        false,
        false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(reverse.reason_code, "routing.to_home");
}

#[test]
fn routing_notice_same_device_is_silent_and_invalid_event_is_rejected() {
    let identity = OwnerIdentity::new("claude", "7001");
    let same = event(&identity, 8, "A", "A");
    assert_eq!(
        prepare(&same, Labels { home: "A", to: "A" }, false, false),
        Ok(None)
    );
    let invalid = event(&identity, -1, "A", "B");
    assert!(prepare(&invalid, Labels { home: "A", to: "B" }, false, false).is_err());
}
