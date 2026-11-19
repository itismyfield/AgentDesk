use super::model::Repository;
use super::profile::{CheckKind, EnvironmentComponent};
use super::verification_run::{
    CommandPolicy, PhaseKind, PhaseTemplate, SelectionRule, SpecCheck, VerificationSpec,
};

pub const ADK_NON_PG: &str = "adk-non-pg";

pub fn adk_verification_spec() -> VerificationSpec {
    use PhaseKind::*;
    let phases = [
        ("archive", Archive, &[][..]),
        ("list", List, &["archive"][..]),
        ("source-compare", SourceCompare, &["list"][..]),
        ("classify", Classify, &["source-compare", "list"][..]),
        ("plan", Plan, &["classify", "archive", "list"][..]),
        ("test", Test, &["plan"][..]),
        ("audit", Audit, &["test", "plan"][..]),
    ];
    VerificationSpec {
        version: "adk-nextest-v1".into(),
        repository: Repository::Adk,
        checks: vec![SpecCheck {
            id: ADK_NON_PG.into(),
            kind: CheckKind::Test,
            required: true,
            acceptance_ids: Vec::new(),
            selection: SelectionRule::NonPgClassifier,
            phases: phases
                .into_iter()
                .map(|(id, kind, requires)| PhaseTemplate {
                    id: id.into(),
                    kind,
                    requires: requires.iter().map(|id| (*id).into()).collect(),
                })
                .collect(),
        }],
        environment: vec![
            EnvironmentComponent::Toolchain,
            EnvironmentComponent::RustTarget,
        ],
        command_policy: CommandPolicy {
            nextest_profile: "ci".into(),
            no_tests_fail: true,
            workers: 1,
        },
    }
}

pub(super) fn landing_prepush_mode(
    mode: crate::domain::campaign::landing::PreflightMode,
) -> &'static [&'static str] {
    use crate::domain::campaign::landing::PreflightMode;
    match mode {
        PreflightMode::Full => &[],
        PreflightMode::PreTlc => &["--static-only"],
    }
}
