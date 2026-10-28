use crate::test_support::input_owner::{Op, Scope};

#[test]
#[cfg(unix)]
fn s5_b0_real_herdr_socket_text_paste_line_and_enter_are_observed() {
    use super::super::herdr::{
        contract::HerdrTransport,
        model::{HerdrCall, HerdrEndpoint, HerdrRequest},
        provenance::{ProcessStart, StartIdentity},
        transport::{HerdrSocketConfig, HerdrSocketTransport},
        wire::LineJsonFraming,
    };
    use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, PANE};
    let _env = crate::config::TestRuntimeRootGuard::new();
    let rig = HerdrRig::start();
    let (_root, attempt, guard) = crate::test_support::input_owner::fixture(
        "herdr",
        &format!("{}#{PANE}", rig.socket().display()),
    );
    let endpoint = HerdrEndpoint::new(
        "fixture-node",
        "fixture-endpoint",
        rig.socket(),
        "fixture-session",
    )
    .unwrap();
    let transport =
        HerdrSocketTransport::new(&endpoint, HerdrSocketConfig::default(), LineJsonFraming)
            .with_peer_reader(Box::new(|_| {
                Ok((
                    100,
                    ProcessStart {
                        identity: StartIdentity::Darwin {
                            seconds: 1000,
                            micros: 0,
                        },
                        wall_clock: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1000),
                    },
                ))
            }));
    let witness = transport.server_witness().unwrap();
    let call = |request| HerdrCall {
        id: "s5-b0".into(),
        request,
    };
    let text = || {
        call(HerdrRequest::PaneSendText {
            pane_id: PANE.into(),
            text: "owner".into(),
        })
    };
    assert!(transport.call_with_witness(&text(), &witness).is_err());
    assert!(rig.sends().is_empty());
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        transport.call_with_witness(&text(), &witness).unwrap();
        transport
            .call_with_witness(
                &call(HerdrRequest::PaneSendText {
                    pane_id: PANE.into(),
                    text: "\u{1b}[200~owner\u{1b}[201~".into(),
                }),
                &witness,
            )
            .unwrap();
        transport
            .call_with_witness(
                &call(HerdrRequest::PaneSendInput {
                    pane_id: PANE.into(),
                    text: "owner".into(),
                    keys: vec!["enter".into()],
                }),
                &witness,
            )
            .unwrap();
        transport
            .call_with_witness(
                &call(HerdrRequest::PaneSendKeys {
                    pane_id: PANE.into(),
                    keys: vec!["enter".into()],
                }),
                &witness,
            )
            .unwrap();
    }
    assert_eq!(guard.counts().owner_content.get(&attempt), Some(&3));
    assert_eq!(guard.counts().attempted_legacy, 1);
    assert_eq!(rig.sends().len(), 4);
    assert_eq!(
        guard
            .counts()
            .events
            .iter()
            .filter(|event| event.op == Op::Paste && event.written)
            .count(),
        1
    );
}
