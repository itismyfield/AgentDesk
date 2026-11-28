//! One authenticated native prefix with the analysis of its last input. The memo lives and dies
//! with the seed, so a reopened source never answers from an older file.
use super::super::super::input_facts::native::{
    Barrier, Evidence, MembershipReceipt, Projection, Registration, terminal::RawWitness,
};
use crate::services::tui_o::shadow::CapturedRecord;
use std::sync::Mutex;

pub(super) type Analyzed = Result<(Vec<Evidence>, Vec<Barrier>, Vec<MembershipReceipt>), String>;

#[derive(PartialEq)]
struct Key {
    through: u64,
    registrations: Vec<u8>,
    raw: Option<RawWitness>,
}

#[derive(Default)]
struct Memo {
    kept: Option<(Key, Analyzed)>,
    /// Inputs reduced through this seed, for the cost tests.
    #[cfg(any(test, feature = "test-support"))]
    reduced: usize,
}

pub(super) struct Seed {
    projection: Result<Projection, String>,
    memo: Mutex<Memo>,
    pub(super) quiet: Option<super::QuietOffer>,
}

impl Seed {
    pub(super) fn new(projection: Result<Projection, String>) -> Self {
        Self {
            projection,
            memo: Mutex::default(),
            quiet: None,
        }
    }

    pub(super) fn projection(&self) -> Result<&Projection, String> {
        self.projection.as_ref().map_err(Clone::clone)
    }

    /// A record the prefix cannot take ends the seed until its source is reopened.
    pub(super) fn append(&mut self, record: &CapturedRecord) {
        if let Ok(projection) = &mut self.projection
            && let Err(error) = projection.append(record)
        {
            self.projection = Err(error);
        }
    }

    /// The closed analysis and receipt candidates, reduced once per input: they are a function
    /// of the prefix, every registration field in order and the whole raw witness.
    pub(super) fn analyzed(
        &self,
        registrations: &[Registration],
        raw: Option<&RawWitness>,
    ) -> Analyzed {
        let projection = self.projection()?;
        let reduce = || -> Analyzed {
            let analysis = projection.analyze_closed(registrations, raw)?;
            let receipts = projection.receipts(registrations);
            Ok((analysis.evidence, analysis.barriers, receipts))
        };
        let Some(registrations) = spelled(registrations) else {
            return reduce();
        };
        let key = Key {
            through: projection.through(),
            registrations,
            raw: raw.cloned(),
        };
        let mut memo = self.memo.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((kept, analyzed)) = &memo.kept
            && *kept == key
        {
            return analyzed.clone();
        }
        #[cfg(any(test, feature = "test-support"))]
        {
            memo.reduced += 1;
        }
        let analyzed = reduce();
        memo.kept = Some((key, analyzed.clone()));
        analyzed
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn reduced(&self) -> usize {
        self.memo.lock().unwrap_or_else(|e| e.into_inner()).reduced
    }
}

/// Every field of every registration in order, as it is stored. Derived equality would fold
/// path spellings that a barrier digest tells apart, and serialization alone skips derived fields.
fn spelled(registrations: &[Registration]) -> Option<Vec<u8>> {
    let fields: Vec<_> = registrations
        .iter()
        .map(|registration| {
            let Registration {
                namespace,
                ticket,
                frame_hash,
                devices,
                exact_frame_hash,
                receipts,
                candidate,
                not_sent,
                barrier,
            } = registration;
            (
                namespace,
                ticket,
                frame_hash,
                exact_frame_hash,
                receipts,
                candidate,
                not_sent,
                barrier,
                devices,
            )
        })
        .collect();
    serde_json::to_vec(&fields).ok()
}

#[cfg(test)]
#[path = "observe_memo_tests.rs"]
mod tests;
