use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OffReason {
    NotInstalled,
    MissingKey,
    GlobalDisabled,
    FeatureDisabled,
    RepositoryDisabled,
    PolicyUnavailable,
}

#[derive(Debug, PartialEq)]
pub(crate) enum DecisionResult {
    Off(OffReason),
    NoAnswer(NoAnswerReason),
    Answer {
        choice: DecisionValue,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
    },
}

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

#[derive(Debug, PartialEq)]
pub(crate) enum DecisionValue {
    #[cfg(test)]
    Noul {
        answer: bool,
        p_yes: f64,
    },
    Choice(String),
    Score(f64),
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Feature {
    IssueDuplicate,
    #[cfg(test)]
    HostAssignment,
    #[cfg(test)]
    RoutinePrecheck,
}

#[derive(Debug, Clone)]
pub(crate) struct Scope {
    pub(crate) feature: Feature,
    pub(crate) repository: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum Question {
    #[cfg(test)]
    Noul {
        instructions: String,
        true_description: Option<String>,
        false_description: Option<String>,
    },
    Choice {
        instructions: String,
        options: Vec<(String, String)>,
    },
    #[cfg(test)]
    Score {
        instructions: String,
        levels: Vec<String>,
    },
}

pub(crate) struct DecisionRequest {
    pub(crate) scope: Scope,
    pub(crate) template_version: String,
    pub(crate) input_text: String,
    pub(crate) question: Question,
}

#[derive(Debug, PartialEq)]
pub(crate) struct DecisionObservation {
    pub(crate) template_version: String,
    pub(crate) elapsed_ms: u64,
    pub(crate) input_tokens: Option<u64>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct DecisionAttempt {
    pub(crate) result: DecisionResult,
    pub(crate) observation: DecisionObservation,
}
