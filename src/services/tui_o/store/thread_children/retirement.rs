//! The request contract only: row deletion grants no authority to delete history or stop writers.

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    child: u64,
    parent: u64,
    init_digest: String,
    row_nonce: String,
    generation: u64,
}

struct RetirementRequest(Identity);

impl RetirementRequest {
    fn after_exact_delete(expected: &Identity, deleted: Option<&Identity>) -> Option<Self> {
        (deleted == Some(expected)).then(|| Self(expected.clone()))
    }

    fn still_targets(&self, current: &Identity) -> bool {
        &self.0 == current
    }
}

#[test]
fn thread_children_retirement_request_requires_exact_success_and_cannot_target_successor() {
    let identity = Identity {
        child: 43,
        parent: 42,
        init_digest: "init-bytes".into(),
        row_nonce: "row-nonce".into(),
        generation: 1,
    };
    assert!(RetirementRequest::after_exact_delete(&identity, None).is_none());
    let request = RetirementRequest::after_exact_delete(&identity, Some(&identity)).unwrap();
    assert!(request.still_targets(&identity));
    for changed in [
        Identity {
            child: 42,
            ..identity.clone()
        },
        Identity {
            child: 45,
            ..identity.clone()
        },
        Identity {
            parent: 44,
            ..identity.clone()
        },
        Identity {
            generation: 2,
            ..identity.clone()
        },
        Identity {
            row_nonce: "successor".into(),
            ..identity.clone()
        },
        Identity {
            init_digest: "another-init".into(),
            ..identity.clone()
        },
    ] {
        assert!(!request.still_targets(&changed));
        assert!(RetirementRequest::after_exact_delete(&identity, Some(&changed)).is_none());
    }
}
