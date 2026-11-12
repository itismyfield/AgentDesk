use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffReason {
    NotInstalled,
    MissingKey,
    GlobalDisabled,
    FeatureDisabled,
    RepositoryDisabled,
    PolicyUnavailable,
}

#[derive(Debug, PartialEq)]
pub enum DecisionResult {
    Off(OffReason),
    NoAnswer(NoAnswerReason),
    Answer {
        choice: DecisionValue,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoAnswerReason {
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
pub enum DecisionValue {
    Noul {
        answer: bool,
        p_yes: f64,
    },
    Choice(String),
    Score(f64),
}

#[derive(Debug, Clone, Copy)]
pub enum Feature {
    IssueDuplicate,
    HostAssignment,
    RoutinePrecheck,
}

#[derive(Debug, Clone)]
pub struct Scope {
    pub feature: Feature,
    pub repository: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Question {
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

pub struct DecisionRequest {
    pub scope: Scope,
    pub template_version: String,
    pub input_text: String,
    pub question: Question,
}

#[derive(Debug, PartialEq)]
pub struct DecisionObservation {
    pub template_version: String,
    pub elapsed_ms: u64,
    pub input_tokens: Option<u64>,
}

#[derive(Debug, PartialEq)]
pub struct DecisionAttempt {
    pub result: DecisionResult,
    pub observation: DecisionObservation,
}
