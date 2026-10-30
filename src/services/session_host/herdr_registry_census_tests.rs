use super::super::herdr::contract::{HerdrTransportError, ServerWitness};
use super::super::herdr::model::{
    HERDR_PROTOCOL, HerdrPane, HerdrReply, HerdrRequest, HerdrResult, HerdrSnapshot,
};
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CensusTransport {
    panes: Vec<String>,
    fault: &'static str,
    reads: AtomicUsize,
}
impl HerdrTransport for CensusTransport {
    fn call(&self, call: &HerdrCall) -> (HerdrOutcome, Witnessed) {
        assert!(matches!(call.request, HerdrRequest::SessionSnapshot {}));
        self.reads.fetch_add(1, Ordering::SeqCst);
        let witness = if self.fault == "witness" {
            ServerWitness::for_test(9)
        } else {
            ServerWitness::for_test(8)
        };
        let result = if self.fault == "partial" {
            return (
                Err(HerdrTransportError::NotSent("partial frame".into())),
                Ok(witness),
            );
        } else if self.fault == "result" {
            HerdrResult::Other
        } else {
            HerdrResult::SessionSnapshot {
                snapshot: HerdrSnapshot {
                    protocol: if self.fault == "protocol" {
                        0
                    } else {
                        HERDR_PROTOCOL
                    },
                    version: "0.9.3".into(),
                    workspaces: None,
                    panes: self
                        .panes
                        .iter()
                        .map(|pane| HerdrPane {
                            pane_id: pane.clone(),
                            workspace_id: "w1".into(),
                            tab_id: "t1".into(),
                            revision: 1,
                            cwd: None,
                            foreground_cwd: None,
                        })
                        .collect(),
                },
            }
        };
        (
            Ok(HerdrReply {
                id: if self.fault == "id" {
                    "other".into()
                } else {
                    call.id.clone()
                },
                body: Ok(result),
            }),
            Ok(witness),
        )
    }
    fn call_with_witness(&self, _: &HerdrCall, _: &ServerWitness) -> HerdrOutcome {
        panic!("census must never mutate")
    }
    fn server_witness(&self) -> Witnessed {
        Ok(ServerWitness::for_test(8))
    }
}
fn registry(panes: &[&str], fault: &'static str) -> (HerdrRegistry, Arc<CensusTransport>) {
    let transport = Arc::new(CensusTransport {
        panes: panes.iter().map(|p| p.to_string()).collect(),
        fault,
        reads: AtomicUsize::new(0),
    });
    let endpoint = HerdrEndpoint::new(
        "local",
        "local",
        std::path::Path::new("/tmp/herdr-test.sock"),
        "agentdesk",
    )
    .unwrap();
    (
        HerdrRegistry::with_transport(endpoint, transport.clone()),
        transport,
    )
}

#[test]
fn complete_registered_census_preserves_extra_pane_and_exact_budget() {
    let (registry, transport) = registry(&["w1-2", "w1-1"], "");
    let census = registry.read_pane_census(3).unwrap();
    assert_eq!(census.len(), 1);
    assert_eq!(census[0].1, ["w1-1", "w1-2"]);
    assert_eq!(transport.reads.load(Ordering::SeqCst), 1);
    assert!(
        registry.read_pane_census(2).is_err(),
        "truncated census cannot be Complete"
    );
}

#[test]
fn partial_mismatched_witness_protocol_id_and_duplicate_are_not_complete() {
    for fault in ["partial", "witness", "protocol", "id", "result"] {
        assert!(
            registry(&["w1-1"], fault).0.read_pane_census(10).is_err(),
            "{fault}"
        );
    }
    assert!(
        registry(&["w1-1", "w1-1"], "")
            .0
            .read_pane_census(10)
            .is_err()
    );
    assert!(registry(&["w1-1"], "").0.read_pane_census(0).is_err());
}
