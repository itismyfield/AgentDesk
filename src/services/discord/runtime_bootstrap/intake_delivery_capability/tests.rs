use super::*;
use std::collections::BTreeSet;
use std::path::PathBuf;

#[test]
fn sql_tokenizer_preserves_literals_and_strips_only_terminal_not_valid() {
    assert_ne!(
        sql_tokens("CHECK(status='dis patched')"),
        sql_tokens("CHECK(status='dispatched')")
    );
    assert_eq!(
        sql_tokens("CHECK(name='it''s ok') NOT VALID"),
        sql_tokens("CHECK(name='it''s ok')")
    );
    assert_ne!(
        sql_tokens("CHECK(\"a b\"='x')"),
        sql_tokens("CHECK(\"ab\"='x')")
    );
    let status_tokens = sql_tokens(INTAKE_CHECKS[1].1);
    let official = crate::db::intake_outbox_status::IntakeOutboxStatus::ALL;
    assert_eq!(
        status_tokens
            .iter()
            .filter(|token| token.starts_with('\''))
            .count(),
        official.len()
    );
    for status in official {
        assert!(status_tokens.contains(&format!("'{}'", status.as_str())));
    }
}

#[test]
fn settlement_capabilities_split_stamp_from_future_settle_and_sweep() {
    assert_eq!(
        capabilities_for(IntakeDeliverySettlementStage::Off, SchemaReason::Ready),
        SettlementCapabilities::default()
    );
    assert_eq!(
        capabilities_for(IntakeDeliverySettlementStage::Observe, SchemaReason::Ready),
        SettlementCapabilities::default()
    );
    assert_eq!(
        capabilities_for(IntakeDeliverySettlementStage::Settle, SchemaReason::Ready),
        SettlementCapabilities {
            stamp_dispatched: false,
            settle_and_sweep: true,
        }
    );
    assert_eq!(
        capabilities_for(IntakeDeliverySettlementStage::Enforce, SchemaReason::Ready),
        SettlementCapabilities {
            stamp_dispatched: true,
            settle_and_sweep: true,
        }
    );
    for stage in [
        IntakeDeliverySettlementStage::Off,
        IntakeDeliverySettlementStage::Observe,
        IntakeDeliverySettlementStage::Settle,
        IntakeDeliverySettlementStage::Enforce,
    ] {
        let capabilities = capabilities_for(stage, SchemaReason::Constraint);
        assert!(!capabilities.stamp_dispatched);
        assert!(!capabilities.settle_and_sweep);
    }
}

#[test]
fn enforce_ready_enables_dispatched_stamping_after_sweep_ships() {
    assert!(
        capabilities_for(IntakeDeliverySettlementStage::Enforce, SchemaReason::Ready)
            .stamp_dispatched
    );
    for stage in [
        IntakeDeliverySettlementStage::Off,
        IntakeDeliverySettlementStage::Observe,
        IntakeDeliverySettlementStage::Settle,
    ] {
        assert!(!capabilities_for(stage, SchemaReason::Ready).stamp_dispatched);
    }
}

#[test]
fn required_migrations_are_a_subset_of_the_migrations_dir() {
    let migrations = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations/postgres");
    let available: BTreeSet<i64> = std::fs::read_dir(migrations)
        .expect("read postgres migrations")
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .split('_')
                .next()
                .and_then(|version| version.parse().ok())
        })
        .collect();
    for version in
        crate::db::intake_delivery_required_migrations::INTAKE_DELIVERY_REQUIRED_MIGRATIONS
    {
        assert!(
            available.contains(&version),
            "missing migration {version:04}"
        );
    }
}

#[test]
fn required_migrations_cover_every_schema_object_the_stamp_touches() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let combined =
        crate::db::intake_delivery_required_migrations::INTAKE_DELIVERY_REQUIRED_MIGRATIONS
            .into_iter()
            .map(|version| {
                let prefix = format!("{version:04}_");
                let path = std::fs::read_dir(root.join("migrations/postgres"))
                    .expect("read postgres migrations")
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .find(|path| {
                        path.file_name()
                            .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
                    })
                    .unwrap_or_else(|| panic!("migration {version:04} must exist"));
                std::fs::read_to_string(path).expect("read required migration")
            })
            .collect::<Vec<_>>()
            .join("\n");
    for identifier in [
        "delivery_journal_events",
        "intake_outbox",
        "dispatched_at",
        "intake_outbox_dispatched_requires_clock",
        "idx_intake_outbox_stale_dispatched",
        "idx_delivery_journal_intake_binding",
    ] {
        assert!(
            combined.contains(identifier),
            "required migrations must cover {identifier}"
        );
    }
}

#[test]
fn capability_probe_binds_exactly_the_shared_constant() {
    let source = include_str!("../intake_delivery_capability.rs");
    let production = source
        .split_once("#[cfg(test)]")
        .expect("capability module has tests")
        .0;
    let constant_name = ["INTAKE_DELIVERY_REQUIRED", "_MIGRATIONS"].concat();
    assert_eq!(production.matches(&constant_name).count(), 1);
    assert!(!production.contains("[103_i64, 105, 106, 107, 108, 109]"));
}
