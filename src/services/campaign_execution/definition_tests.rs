use super::*;
use serde_json::{Value, json};

fn input() -> DefinitionInput {
    serde_json::from_value(json!({
        "repository": "adk", "title": "Persistence", "goal": "Keep accepted work across restart",
        "profile_version": "v1",
        "nodes": [
            {"id": "a", "title": "Implement", "issue_url": "https://github.com/itismyfield/AgentDesk/issues/7438",
             "acceptance": [{"id": "restart", "description": "Accepted state survives restart", "verification": "test"},
                            {"id": "deploy", "description": "Observe deployed revision", "verification": "deployment"}],
             "dependencies": [], "group": null},
            {"id": "b", "title": "Verify", "issue_url": "https://github.com/itismyfield/AgentDesk/issues/7438",
             "acceptance": [{"id": "restart", "description": "Verify main revision", "verification": "main"}],
             "dependencies": ["a"], "group": "verification"}
        ]
    })).unwrap()
}

fn rejection(
    mut change: impl FnMut(&mut DefinitionInput),
    code: DefinitionErrorCode,
    target: &str,
) {
    let mut value = input();
    change(&mut value);
    let error = validate_definition(value).unwrap_err();
    assert_eq!(error.code, code);
    assert_eq!(error.target.as_deref(), Some(target));
}

#[test]
fn definition_rejects_unknown_and_execution_fields_at_every_input_level() {
    let original = serde_json::to_value(input()).unwrap();
    for field in [
        "status",
        "stage",
        "round",
        "assignee",
        "session",
        "session_id",
        "provider",
        "evidence",
        "head",
        "auto_queue",
        "acceptance_complete",
        "unexpected",
    ] {
        for node in [false, true] {
            let mut body = original.clone();
            let target = if node {
                &mut body["nodes"][0]
            } else {
                &mut body
            };
            target[field] = Value::Bool(true);
            let error = serde_json::from_value::<DefinitionInput>(body)
                .unwrap_err()
                .to_string();
            assert!(error.contains("unknown field"), "{field}: {error}");
        }
        let mut body = original.clone();
        body["nodes"][0]["acceptance"][0][field] = Value::Bool(true);
        assert!(
            serde_json::from_value::<DefinitionInput>(body)
                .unwrap_err()
                .to_string()
                .contains("unknown field")
        );
    }
}

#[test]
fn definition_rejects_empty_acceptance_and_duplicate_stable_ids() {
    use DefinitionErrorCode as C;
    rejection(|d| d.nodes[0].acceptance.clear(), C::EmptyAcceptance, "a");
    rejection(
        |d| d.nodes[0].acceptance[0].description = " ".into(),
        C::InvalidField,
        "restart",
    );
    rejection(
        |d| d.nodes[0].acceptance[1].id = "restart".into(),
        C::DuplicateAcceptance,
        "restart",
    );
    rejection(|d| d.nodes[1].id = "a".into(), C::DuplicateNode, "a");
    // AC identity is scoped to its node, so separate tasks can use the same stable AC ID.
    assert!(validate_definition(input()).is_ok());
}

#[test]
fn definition_checks_identity_profile_and_required_text_for_both_repositories() {
    use DefinitionErrorCode as C;
    for id in ["", "white space", "../path", "한글"] {
        rejection(|d| d.nodes[0].id = id.into(), C::InvalidField, id);
        rejection(
            |d| d.nodes[0].acceptance[0].id = id.into(),
            C::InvalidField,
            id,
        );
    }
    for url in [
        "not a URL",
        "http://github.com/o/r/issues/1",
        "https://example.com/o/r/issues/1",
        "https://github.com/o/r/pull/1",
        "https://github.com/o/r/issues/0",
    ] {
        rejection(|d| d.nodes[0].issue_url = url.into(), C::InvalidField, "a");
    }
    rejection(|d| d.goal = " ".into(), C::InvalidField, "definition");
    rejection(|d| d.nodes.clear(), C::InvalidField, "definition");
    for repository in [Repository::Adk, Repository::CookingHeart] {
        let mut definition = input();
        definition.repository = repository;
        assert!(validate_definition(definition.clone()).is_ok());
        definition.profile_version = "unsupported".into();
        assert_eq!(
            validate_definition(definition).unwrap_err().code,
            C::UnsupportedProfile
        );
    }
    let mut body = serde_json::to_value(input()).unwrap();
    body["repository"] = json!("third_repository");
    assert!(serde_json::from_value::<DefinitionInput>(body).is_err());
}

#[test]
fn definition_rejects_missing_and_repeated_dependencies_with_targets() {
    for (dependencies, code, path) in [
        (
            vec!["absent"],
            DefinitionErrorCode::MissingDependency,
            "absent",
        ),
        (vec!["a", "a"], DefinitionErrorCode::RepeatedDependency, "a"),
    ] {
        let mut value = input();
        value.nodes[1].dependencies = dependencies.into_iter().map(String::from).collect();
        let error = validate_definition(value).unwrap_err();
        assert_eq!(error.code, code);
        assert_eq!(error.target.as_deref(), Some("b"));
        assert_eq!(error.path, [path]);
    }
}

#[test]
fn definition_cycle_path_excludes_downstream_residue_and_closes_actual_back_edge() {
    let mut value = input();
    value.nodes[0].dependencies = vec!["b".into()];
    let mut downstream = value.nodes[0].clone();
    downstream.id = "downstream".into();
    downstream.dependencies = vec!["a".into()];
    value.nodes.insert(0, downstream);
    let error = validate_definition(value.clone()).unwrap_err();
    assert_eq!(error.code, DefinitionErrorCode::Cycle);
    assert_eq!(error.path, ["a", "b", "a"]);
    for pair in error.path.windows(2) {
        assert!(
            value
                .nodes
                .iter()
                .find(|n| n.id == pair[0])
                .unwrap()
                .dependencies
                .contains(&pair[1])
        );
    }
    value = input();
    value.nodes[0].dependencies = vec!["a".into()];
    assert_eq!(validate_definition(value).unwrap_err().path, ["a", "a"]);
}

#[test]
fn definition_allows_separate_nodes_for_one_issue_without_merging_their_acceptance() {
    let validated = validate_definition(input()).unwrap();
    assert_eq!(validated.input().nodes.len(), 2);
    assert_eq!(
        validated.input().nodes[0].issue_url,
        validated.input().nodes[1].issue_url
    );
    assert_ne!(
        validated.hashes()["a"].acceptance,
        validated.hashes()["b"].acceptance
    );
    assert!(validated.superseded_nodes(&validated).is_empty());
}

#[test]
fn definition_hashes_invalidate_only_changed_requirement_acceptance_or_profile() {
    let original = input();
    let old = validate_definition(original.clone()).unwrap();
    let mut requirements = original.clone();
    requirements.nodes[0].issue_url = "https://github.com/itismyfield/AgentDesk/issues/1".into();
    let changed = validate_definition(requirements).unwrap();
    assert_ne!(
        old.hashes()["a"].requirements,
        changed.hashes()["a"].requirements
    );
    assert_eq!(
        old.hashes()["a"].acceptance,
        changed.hashes()["a"].acceptance
    );
    assert_eq!(old.hashes()["a"].profile, changed.hashes()["a"].profile);
    assert_eq!(old.superseded_nodes(&changed), ["a"]);
    let mut acceptance = original.clone();
    acceptance.nodes[1].acceptance[0].verification = VerificationKind::Operational;
    let changed = validate_definition(acceptance).unwrap();
    assert_eq!(
        old.hashes()["b"].requirements,
        changed.hashes()["b"].requirements
    );
    assert_ne!(
        old.hashes()["b"].acceptance,
        changed.hashes()["b"].acceptance
    );
    assert_eq!(old.hashes()["b"].profile, changed.hashes()["b"].profile);
    assert_eq!(old.superseded_nodes(&changed), ["b"]);
    let mut goal = original.clone();
    goal.goal.push_str(" with new requirements");
    assert_eq!(
        old.superseded_nodes(&validate_definition(goal).unwrap()),
        ["a", "b"]
    );
    let mut profile = original;
    profile.repository = Repository::CookingHeart;
    let changed = validate_definition(profile).unwrap();
    assert_ne!(old.hashes()["a"].profile, changed.hashes()["a"].profile);
    assert_eq!(
        old.hashes()["a"].requirements,
        changed.hashes()["a"].requirements
    );
    assert_eq!(
        old.hashes()["a"].acceptance,
        changed.hashes()["a"].acceptance
    );
    assert_eq!(old.superseded_nodes(&changed), ["a", "b"]);
}

#[test]
fn definition_display_edges_and_ordering_preserve_existing_code_evidence_hashes() {
    let old = validate_definition(input()).unwrap();
    let mut display = input();
    display.title = "Renamed campaign".into();
    display.nodes[0].title = "Renamed task".into();
    display.nodes[0].group = Some("other group".into());
    display.nodes[0].acceptance.reverse();
    display.nodes.reverse();
    let changed = validate_definition(display.clone()).unwrap();
    assert_eq!(old.hashes(), changed.hashes());
    assert!(!graph_changed(&old, &changed));
    assert!(old.superseded_nodes(&changed).is_empty());
    display.nodes[0].dependencies.clear();
    let changed = validate_definition(display).unwrap();
    assert_eq!(old.hashes(), changed.hashes());
    assert!(graph_changed(&old, &changed));
    assert!(old.superseded_nodes(&changed).is_empty());
}

#[test]
fn definition_long_dag_uses_iterative_cycle_detection() {
    let template = input().nodes.remove(0);
    let mut definition = input();
    definition.nodes = (0..1000)
        .map(|i| {
            let mut node = template.clone();
            node.id = format!("n{i}");
            node.dependencies = if i == 999 {
                vec![]
            } else {
                vec![format!("n{}", i + 1)]
            };
            node
        })
        .collect();
    assert!(validate_definition(definition.clone()).is_ok());
    definition.nodes[999].dependencies = vec!["n998".into()];
    assert_eq!(
        validate_definition(definition).unwrap_err().path,
        ["n998", "n999", "n998"]
    );
}
