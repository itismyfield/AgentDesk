use super::publication::Observations;
use super::*;
use std::sync::Mutex;

fn publication() -> BootPublication {
    let selection = BootSelection {
        runtime_kind: "codex_tui".into(),
        turn_channels: [7].into(),
    };
    BootPublication::new(
        1,
        [
            ("codex".to_owned(), selection.clone()),
            ("claude".to_owned(), selection),
        ]
        .into(),
        Arc::new(Mutex::new(Observations::default())),
    )
}

#[test]
fn publication_is_provider_scoped_and_sealed() {
    let mut publication = publication();
    let mut effects = Vec::new();
    publication
        .confirm("claude", &mut |provider, permission| {
            assert!(
                permission
                    .publish_with(2, provider, 7, || {
                        effects.push("epoch");
                        Ok(())
                    })
                    .is_err()
            );
            assert!(
                permission
                    .publish_with(1, "codex", 7, || {
                        effects.push("scope");
                        Ok(())
                    })
                    .is_err()
            );
            assert!(
                permission
                    .publish_with(1, provider, 8, || {
                        effects.push("selection");
                        Ok(())
                    })
                    .is_err()
            );
            permission.publish_with(1, provider, 7, || {
                effects.push("claude");
                Ok(())
            })
        })
        .unwrap();
    publication
        .confirm("codex", &mut |provider, permission| {
            permission.publish_with(1, provider, 7, || {
                effects.push("codex");
                Ok(())
            })
        })
        .unwrap();
    let receipt = publication.seal().unwrap();
    assert!(receipt.matches(1));
    assert!(!receipt.matches(2));
    assert!(
        publication
            .publish_with(1, "codex", 7, || {
                effects.push("sealed");
                Ok(())
            })
            .is_err()
    );
    assert_eq!(effects, ["claude", "codex"]);
    assert!(publication.confirm("codex", &mut |_, _| Ok(())).is_err());
}

#[test]
fn seal_requires_every_provider_result() {
    let mut publication = publication();
    assert!(publication.seal().is_err());
    publication
        .confirm("claude", &mut |_, permission| {
            assert!(permission.seal().is_err());
            Ok(())
        })
        .unwrap();
    assert!(publication.seal().is_err());
    publication
        .confirm("codex", &mut |_, permission| {
            assert!(permission.seal().is_err());
            Ok(())
        })
        .unwrap();
    assert!(publication.seal().is_ok());
    assert!(publication.seal().is_err());
}

#[test]
fn explicit_refusal_does_not_fabricate_publication() {
    let observations = Arc::new(Mutex::new(Observations::default()));
    let selection = BootSelection {
        runtime_kind: "codex_tui".into(),
        turn_channels: [7].into(),
    };
    let mut publication = BootPublication::new(
        1,
        [("codex".to_owned(), selection)].into(),
        observations.clone(),
    );
    publication
        .confirm("codex", &mut |provider, permission| {
            permission.publish_with(1, provider, 7, || Err("protected".into()))
        })
        .unwrap();
    assert!(publication.seal().is_ok());
    let report = observations.lock().unwrap();
    assert!(report.published.is_empty());
    assert_eq!(report.refused, [(("codex".into(), 7), "protected".into())]);
}
