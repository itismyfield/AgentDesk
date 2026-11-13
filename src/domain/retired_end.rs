//! Opaque evidence naming the exact value whose termination was observed.

#[derive(Debug)]
pub(crate) struct RetiredEnd<T>(T);

impl<T: PartialEq> RetiredEnd<T> {
    pub(crate) fn matches(&self, record: &T) -> bool {
        self.0 == *record
    }
}

#[path = "../services/session_host/retired_end_issuer.rs"]
pub(crate) mod issuer;
