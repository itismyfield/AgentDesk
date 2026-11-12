#[cfg(test)]
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OffReason {
    NotInstalled,
    #[cfg(test)]
    MissingKey,
    #[cfg(test)]
    GlobalDisabled,
    #[cfg(test)]
    FeatureDisabled,
    #[cfg(test)]
    RepositoryDisabled,
    #[cfg(test)]
    PolicyUnavailable,
}

#[derive(Debug, PartialEq)]
pub(crate) enum DecisionResult {
    Off(OffReason),
    #[cfg(test)]
    NoAnswer(NoAnswerReason),
    #[cfg(test)]
    Answer {
        choice: DecisionValue,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
    },
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoAnswerReason {
    InvalidRequest,
    Timeout,
    Transport,
    Authentication,
    RateLimited,
    ProviderError,
    Blocked,
    MissingAnswer,
    InvalidResponse,
    ClientUnavailable,
}

#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(crate) enum DecisionValue {
    Noul { answer: bool, p_yes: f64 },
    Choice(String),
    Score(f64),
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum Feature {
    IssueDuplicate,
    HostAssignment,
    RoutinePrecheck,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct Scope {
    pub(crate) feature: Feature,
    pub(crate) repository: Option<String>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) enum Question {
    Noul {
        instructions: String,
        true_description: Option<String>,
        false_description: Option<String>,
    },
    Choice {
        instructions: String,
        options: Vec<(String, String)>,
    },
    Score {
        instructions: String,
        levels: Vec<String>,
    },
}

#[cfg(test)]
pub(crate) struct DecisionRequest {
    pub(crate) scope: Scope,
    pub(crate) template_version: String,
    pub(crate) input_text: String,
    pub(crate) question: Question,
}

#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(crate) struct DecisionObservation {
    pub(crate) template_version: String,
    pub(crate) elapsed_ms: u64,
    pub(crate) input_tokens: Option<u64>,
}

#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(crate) struct DecisionAttempt {
    pub(crate) result: DecisionResult,
    pub(crate) observation: DecisionObservation,
}
