//! Preserve intake's source material and policy result without earning settlement.

use std::io;

use crate::services::discord::input_runtime::intake::{
    self, Classification, PolicyFacts, PreparedSource, SourceDraft,
};

pub(in crate::services::discord::catch_up) struct ClassifiedSource {
    pub source: PreparedSource,
    pub classification: Classification,
}

pub(in crate::services::discord::catch_up) fn classify_prepared(
    facts: &PolicyFacts,
    source: &PreparedSource,
) -> Classification {
    intake::classify(facts, source)
}

pub(in crate::services::discord::catch_up) fn prepare(
    draft: SourceDraft,
    facts: &PolicyFacts,
) -> io::Result<ClassifiedSource> {
    let source = intake::prepare_source(draft)?;
    let classification = classify_prepared(facts, &source);
    Ok(ClassifiedSource {
        source,
        classification,
    })
}
