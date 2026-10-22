use super::*;
use crate::services::session_host::herdr::contract::ServerWitness;

fn consent() -> Consent {
    Consent {
        schema: 1,
        key: super::super::ConsentKey {
            node: "execution-node".into(),
            user: 501,
            socket: "/fixture/socket".into(),
            named_session: "agentdesk".into(),
            config: "/private/home/config.toml".into(),
            target: "/target/config.toml".into(),
        },
        preview: super::super::Preview {
            revision: 1,
            witness: ServerWitness::for_test(7),
            target_hash: [1; 32],
            chain_hash: [2; 32],
            checker_hash: [3; 32],
        },
        owner: 1,
        bot: 2,
        provider: "claude".into(),
        channel: 3,
        message: 0,
        card: CardState::Reserved,
        nonce: "opaque_nonce-123".into(),
        expires: 1900,
        state: State::Pending,
        decision: None,
        intents: Vec::new(),
        apply: None,
        launch: None,
        settlements: Vec::new(),
        intent_previews: Vec::new(),
    }
}

#[test]
fn card_discloses_scope_and_enables_buttons_only_after_durable_binding() {
    let mut consent = consent();
    let card = render(&consent).unwrap();
    for required in [
        "/private/home/config.toml",
        "session.resume_agents_on_restore",
        "false",
        "개인 workspace를 포함한 이 서버 전체",
        "다음 herdr 시작부터",
        "레이아웃 복원은 유지",
        "현재 서버는 재시작하지 않습니다",
    ] {
        assert!(
            card.content.contains(required),
            "card_scope_disclosure_contract: {required}"
        );
    }
    assert_eq!(card.buttons.len(), 2);
    assert_eq!(
        card.buttons
            .iter()
            .map(|button| button.label)
            .collect::<Vec<_>>(),
        ["예", "아니오"]
    );
    assert!(
        card.buttons.iter().all(|button| button.disabled),
        "card_bind_before_enable_contract"
    );
    consent.card = CardState::Bound;
    assert!(
        render(&consent)
            .unwrap()
            .buttons
            .iter()
            .all(|button| button.disabled)
    );
    consent.message = 40;
    let card = render(&consent).unwrap();
    for (button, yes) in card.buttons.iter().zip([true, false]) {
        assert!(!button.disabled, "card_bound_buttons_enabled_contract");
        assert_eq!(
            parse(&button.custom_id),
            Some(Click {
                nonce: consent.nonce.clone(),
                yes
            })
        );
        assert!(!button.custom_id.contains('/'), "card_nonce_only_contract");
    }
    consent.card = CardState::Awaiting;
    assert!(
        render(&consent).unwrap().buttons.is_empty(),
        "card_automatic_no_buttons_contract"
    );
    let revised = revision_changed(&consent).unwrap();
    assert!(
        revised.buttons.is_empty() && revised.content.contains("revision이 변경"),
        "card_revision_change_invalidates_buttons_contract"
    );
}

#[test]
fn card_parser_rejects_other_handlers_versions_paths_and_extra_fields() {
    for invalid in [
        "legacy:yes",
        "hresume:v2:nonce:yes",
        "hresume:v1::yes",
        "hresume:v1:/path:yes",
        "hresume:v1:nonce:yes:extra",
        "hresume:v1:nonce:approve",
        "hresume:v1:한글:no",
    ] {
        assert!(
            parse(invalid).is_none(),
            "card_strict_custom_id_contract: {invalid}"
        );
    }
    assert!(parse(&format!("hresume:v1:{}:yes", "x".repeat(65))).is_none());
    let mut consent = consent();
    consent.nonce = "/private/path".into();
    assert!(render(&consent).is_err());
}

#[test]
fn card_results_remove_buttons_and_preserve_retry_and_launch_distinctions() {
    let mut consent = consent();
    for state in [
        State::Accepted,
        State::Applied,
        State::Denied,
        State::Expired,
        State::ApplyFailed,
    ] {
        consent.state = state.clone();
        let card = render(&consent).unwrap();
        assert!(
            card.buttons.is_empty(),
            "card_terminal_buttons_removed_contract"
        );
        match state {
            State::Accepted => {
                assert!(card.content.contains("완료 전에는 다시 시도할 수 없습니다"))
            }
            State::Applied => {
                assert!(
                    card.content.contains("세션 시작 결과는 별도로"),
                    "card_apply_not_launch_contract"
                );
                for required in ["이미 적용됨", "awaiting_server_restart", "ready"] {
                    assert!(
                        card.content.contains(required),
                        "card_applied_result_contract"
                    );
                }
                assert!(!card.content.contains("!herdr consent retry"));
            }
            _ => {
                assert!(card.content.contains("보류한 채 보존"));
                assert!(
                    card.content
                        .contains("execution-node의 gateway에서 !herdr consent retry")
                );
            }
        }
    }
}

#[test]
fn card_bounds_diagnostics_and_refuses_an_undisplayable_config_path() {
    let mut consent = consent();
    consent.state = State::ApplyFailed;
    consent.apply = Some(super::super::ApplyResult {
        before: None,
        after: None,
        backup: None,
        detail: Some("diagnostic ".repeat(100_000)),
    });
    let card = render(&consent).unwrap();
    assert!(card.content.contains("원인: diagnostic"));
    assert!(
        card.content.encode_utf16().count() <= 1900,
        "card_diagnostics_bounded_contract"
    );
    consent.state = State::Pending;
    consent.key.config = format!("/{}", "x".repeat(3000)).into();
    assert!(
        render(&consent).is_err(),
        "card_undisplayable_path_refused_contract"
    );
}
