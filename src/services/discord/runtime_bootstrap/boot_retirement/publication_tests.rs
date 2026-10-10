use super::super::{BootBot, BootRoster};
use super::*;
use std::sync::Arc;

fn fixture() -> Arc<BootCohort<()>> {
    let selection = BootSelection {
        runtime_kind: "codex_tui".into(),
        turn_channels: [7].into(),
    };
    let bots = ["claude", "codex"].map(|p| BootBot {
        slot: p.into(),
        provider: p.into(),
        utility: false,
        selection: selection.clone(),
    });
    Arc::new(BootCohort::new(1, BootRoster::new(bots.into()).unwrap()))
}
#[test]
fn publication_is_provider_scoped_and_sealed() {
    let cohort = fixture();
    let mut publication = BootPublication::new(&cohort);
    let mut effects = Vec::new();
    publication
        .confirm("claude", &mut |provider, permission| {
            let wrong_epoch = permission.publish_with(2, provider, 7, || {
                effects.push("epoch");
                Ok(())
            });
            assert!(effects.is_empty());
            assert!(wrong_epoch.is_err());
            let wrong_provider = permission.publish_with(1, "codex", 7, || {
                effects.push("scope");
                Ok(())
            });
            assert!(effects.is_empty());
            assert!(wrong_provider.is_err());
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
    // Retain a valid scope in this private fixture to isolate the seal guard.
    publication.current = Some("codex");
    let sealed = publication.publish_with(1, "codex", 7, || {
        effects.push("sealed");
        Ok(())
    });
    assert_eq!(effects, ["claude", "codex"]);
    assert!(sealed.is_err());
    assert!(publication.confirm("codex", &mut |_, _| Ok(())).is_err());
}
#[test]
fn seal_requires_every_provider_result() {
    let cohort = fixture();
    let mut publication = BootPublication::new(&cohort);
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
    let cohort = fixture();
    let mut publication = BootPublication::new(&cohort);
    publication
        .confirm("codex", &mut |provider, permission| {
            permission.publish_with(1, provider, 7, || Err("protected".into()))
        })
        .unwrap();
    assert_eq!(cohort.snapshot().published_keys.len(), 0);
    assert_eq!(
        cohort.snapshot().refused_keys,
        [(("codex".into(), 7), "protected".into())]
    );
    assert!(publication.seal().is_err());
}
