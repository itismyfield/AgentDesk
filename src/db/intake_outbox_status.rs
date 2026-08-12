use std::fmt;
use std::str::FromStr;

/// Stable domain statuses for intake outbox rows.
///
/// The outbound status spelling returned by [`Self::as_str`] and the diagnostic
/// Rust-variant spelling returned by [`Self::variant_name`] are each defined by
/// their respective wildcard-free exhaustive match. Classifications are defined
/// only by the wildcard-free exhaustive matches in [`Self::is_open`] and
/// [`Self::operator_retry`].
///
/// VALIDATES:
/// - The compiler requires every added variant to be handled by those four
///   exhaustive matches. Renaming a variant also invalidates direct references
///   in [`Self::ALL`] and [`Self::from_str`].
/// - The `[Self; 8]` type of [`Self::ALL`] makes the initializer's cardinality a
///   compile-time check.
/// - Unit tests check [`Self::ALL`] membership against the enum declaration,
///   [`Self::from_str`] against [`Self::as_str`], and [`Self::is_open`] against
///   [`INTAKE_OUTBOX_OPEN_STATUSES_SQL`][open-statuses]. They also check
///   [`Self::ALL`] against the 0106 status CHECK body, [`Self::operator_retry`]
///   against `TRANSITION_12_ALLOWED`, and that rejected input is preserved in
///   [`UnknownIntakeStatus`].
///
/// DOES NOT VALIDATE:
/// - The compiler does not require [`Self::from_str`] to gain an arm for an
///   added variant because its match has an unknown-input arm. That coverage and
///   the members (rather than the cardinality) of [`Self::ALL`] are test-backed.
/// - `#[sqlx(rename_all = "snake_case")]` is currently inert: the strong-enum
///   `sqlx::Type` derive supplies `Type<Postgres>` and `PgHasArrayType`, while
///   `rename_all` is consumed by `sqlx::Encode` and `sqlx::Decode` derives, which
///   this type does not have. Adding them would make `rename_all` control their
///   spelling, which must then be reconciled with [`Self::as_str`].
///
/// LIMITS:
/// - No gate prevents replacing an exhaustive classification arm with `_ =>`;
///   preserving wildcard-free matches is a review convention.
/// - The source-based [`Self::ALL`] membership test reads only this file and
///   fails closed on unsupported enum syntax, but no independent gate proves
///   that its defining equality assertion remains present.
/// - These tests compare independent source authorities; they do not apply the
///   migration or exercise PostgreSQL encoding and decoding.
///
/// [open-statuses]: crate::db::intake_outbox_open_status::INTAKE_OUTBOX_OPEN_STATUSES_SQL
#[derive(Clone, Copy, Debug, Eq, PartialEq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub(crate) enum IntakeOutboxStatus {
    Pending,
    Claimed,
    Accepted,
    Spawned,
    Dispatched,
    Done,
    FailedPreAccept,
    FailedPostAccept,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperatorRetryClass {
    ForceTerminate,
    AlreadyTerminal,
    Refuse,
}

impl IntakeOutboxStatus {
    pub(crate) const ALL: [Self; 8] = [
        Self::Pending,
        Self::Claimed,
        Self::Accepted,
        Self::Spawned,
        Self::Dispatched,
        Self::Done,
        Self::FailedPreAccept,
        Self::FailedPostAccept,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Accepted => "accepted",
            Self::Spawned => "spawned",
            Self::Dispatched => "dispatched",
            Self::Done => "done",
            Self::FailedPreAccept => "failed_pre_accept",
            Self::FailedPostAccept => "failed_post_accept",
        }
    }

    pub(crate) const fn variant_name(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Claimed => "Claimed",
            Self::Accepted => "Accepted",
            Self::Spawned => "Spawned",
            Self::Dispatched => "Dispatched",
            Self::Done => "Done",
            Self::FailedPreAccept => "FailedPreAccept",
            Self::FailedPostAccept => "FailedPostAccept",
        }
    }

    pub(crate) const fn is_open(self) -> bool {
        match self {
            Self::Pending | Self::Claimed | Self::Accepted | Self::Spawned | Self::Dispatched => {
                true
            }
            Self::Done | Self::FailedPreAccept | Self::FailedPostAccept => false,
        }
    }

    pub(crate) const fn operator_retry(self) -> OperatorRetryClass {
        match self {
            Self::Accepted | Self::Spawned => OperatorRetryClass::ForceTerminate,
            Self::FailedPostAccept => OperatorRetryClass::AlreadyTerminal,
            Self::Pending
            | Self::Claimed
            | Self::Dispatched
            | Self::Done
            | Self::FailedPreAccept => OperatorRetryClass::Refuse,
        }
    }
}

impl fmt::Display for IntakeOutboxStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("unknown intake outbox status: {0}")]
pub(crate) struct UnknownIntakeStatus(String);

impl FromStr for IntakeOutboxStatus {
    type Err = UnknownIntakeStatus;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pending" => Ok(Self::Pending),
            "claimed" => Ok(Self::Claimed),
            "accepted" => Ok(Self::Accepted),
            "spawned" => Ok(Self::Spawned),
            "dispatched" => Ok(Self::Dispatched),
            "done" => Ok(Self::Done),
            "failed_pre_accept" => Ok(Self::FailedPreAccept),
            "failed_post_accept" => Ok(Self::FailedPostAccept),
            unknown => Err(UnknownIntakeStatus(unknown.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{IntakeOutboxStatus, OperatorRetryClass, UnknownIntakeStatus};
    use crate::db::intake_outbox_open_status::INTAKE_OUTBOX_OPEN_STATUSES_SQL;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    const MODULE_SOURCE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/", file!()));

    fn repo_source(path: &str) -> String {
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path))
            .unwrap_or_else(|error| panic!("read {path}: {error}"))
    }

    fn quoted_sql_values(sql_list: &str) -> BTreeSet<&str> {
        sql_list
            .split(',')
            .map(|value| value.trim().trim_matches('\''))
            .filter(|value| !value.is_empty())
            .collect()
    }

    fn declared_variant_names(source: &str) -> BTreeSet<&str> {
        let body = source
            .split_once("enum IntakeOutboxStatus {")
            .expect("source must declare IntakeOutboxStatus")
            .1
            .split_once("\n}")
            .expect("IntakeOutboxStatus must have a closing brace")
            .0;

        body.lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty()
                    || line.starts_with("//")
                    || line.starts_with("/*")
                    || line.starts_with('*')
                    || line.starts_with("#[")
                {
                    return None;
                }
                line.strip_suffix(',')
                    .filter(|name| name.chars().all(|ch| ch == '_' || ch.is_alphanumeric()))
            })
            .collect()
    }

    #[test]
    fn all_exactly_matches_declared_variants() {
        let declared = declared_variant_names(MODULE_SOURCE);
        let all: BTreeSet<_> = IntakeOutboxStatus::ALL
            .map(IntakeOutboxStatus::variant_name)
            .into_iter()
            .collect();
        assert_eq!(all, declared);

        let decorated = MODULE_SOURCE.replacen(
            "    Pending,",
            "    /// Parser-resilience probe.\n    #[doc = \"pending\"]\n    Pending,",
            1,
        );
        assert_eq!(declared_variant_names(&decorated), declared);
    }

    #[test]
    fn open_classification_matches_shared_sql_list() {
        let classified: BTreeSet<_> = IntakeOutboxStatus::ALL
            .into_iter()
            .filter(|status| status.is_open())
            .map(IntakeOutboxStatus::as_str)
            .collect();
        assert_eq!(
            classified,
            quoted_sql_values(INTAKE_OUTBOX_OPEN_STATUSES_SQL)
        );
    }

    #[test]
    fn all_exactly_matches_migration_status_check() {
        let migration =
            repo_source("migrations/postgres/0106_intake_outbox_dispatched_status_swap.sql");
        let check_values = migration
            .split_once("ADD CONSTRAINT intake_outbox_status_check CHECK (status IN (")
            .expect("migration must add intake_outbox_status_check")
            .1
            .split_once(")) NOT VALID;")
            .expect("status CHECK must remain NOT VALID")
            .0;
        let all: BTreeSet<_> = IntakeOutboxStatus::ALL
            .map(IntakeOutboxStatus::as_str)
            .into_iter()
            .collect();
        assert_eq!(all, quoted_sql_values(check_values));
    }

    #[test]
    fn status_strings_round_trip() {
        for status in IntakeOutboxStatus::ALL {
            assert_eq!(status.as_str().parse(), Ok(status));
            assert_eq!(status.to_string(), status.as_str());
        }
    }

    #[test]
    fn unknown_status_preserves_rejected_spelling() {
        let unknown = "waiting_for_operator";
        assert_eq!(
            unknown.parse::<IntakeOutboxStatus>(),
            Err(UnknownIntakeStatus(unknown.to_owned()))
        );
    }

    #[test]
    fn operator_retry_matches_transition_12_allowed() {
        let intake_outbox = repo_source("src/db/intake_outbox.rs");
        let allowed_source = intake_outbox
            .split_once("const TRANSITION_12_ALLOWED")
            .expect("intake_outbox must declare TRANSITION_12_ALLOWED")
            .1
            .split_once('=')
            .expect("TRANSITION_12_ALLOWED must have an initializer")
            .1
            .split_once(';')
            .expect("TRANSITION_12_ALLOWED must end with a semicolon")
            .0;
        let allowed = allowed_source
            .split('"')
            .skip(1)
            .step_by(2)
            .collect::<BTreeSet<_>>();

        for status in IntakeOutboxStatus::ALL {
            let expected = if status == IntakeOutboxStatus::FailedPostAccept {
                OperatorRetryClass::AlreadyTerminal
            } else if allowed.contains(status.as_str()) {
                OperatorRetryClass::ForceTerminate
            } else {
                OperatorRetryClass::Refuse
            };
            assert_eq!(status.operator_retry(), expected);
            assert_eq!(
                status.operator_retry() != OperatorRetryClass::Refuse,
                allowed.contains(status.as_str())
            );
        }
    }
}
