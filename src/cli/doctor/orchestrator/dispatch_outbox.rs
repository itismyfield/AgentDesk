use super::*;

pub(super) fn check_dispatch_outbox(snapshot: &HealthSnapshot) -> Check {
    let Some(body) = snapshot.body.as_ref() else {
        return Check::fail(
            "dispatch_outbox",
            CheckGroup::Core,
            "Dispatch Outbox",
            "health payload unavailable",
            "Cannot read dispatch outbox health.",
        )
        .with_subsystem("dispatch_outbox")
        .with_fix_safety(FixSafety::NotFixable);
    };
    // A missing block is a read cut at the health deadline, not an empty outbox.
    let Some(stats) = body
        .get("dispatch_outbox")
        .filter(|stats| stats.is_object())
    else {
        let issue = health::measurement::FieldIssue::new(
            "dispatch_outbox",
            body.get("dispatch_outbox"),
            "object",
        );
        return observation_checks::unmeasured_check("dispatch_outbox", &issue)
            .with_path(health_detail_endpoint(&snapshot.base));
    };
    let stats = Some(stats);
    let pending = stats
        .and_then(|v| v.get("pending"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let retrying = stats
        .and_then(|v| v.get("retrying"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let permanent_failures = stats
        .and_then(|v| v.get("permanent_failures"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let oldest_pending_age = stats
        .and_then(|v| v.get("oldest_pending_age"))
        .and_then(Value::as_i64)
        .or_else(|| body.get("outbox_age").and_then(Value::as_i64))
        .unwrap_or(0);
    let detail = format!(
        "pending={pending} retrying={retrying} permanent_failures={permanent_failures} oldest_pending_age={oldest_pending_age}s"
    );
    let evidence = json!({
        "dispatch_outbox": {
            "pending": pending,
            "retrying": retrying,
            "permanent_failures": permanent_failures,
            "oldest_pending_age": oldest_pending_age
        }
    });

    if permanent_failures > 0 {
        Check::fail(
            "dispatch_outbox",
            CheckGroup::Core,
            "Dispatch Outbox",
            detail.clone(),
            "There are permanent dispatch outbox failures. Check the delivery/follow-up paths.",
        )
        .with_subsystem("health")
        .with_severity(Severity::Error)
        .with_path(health_detail_endpoint(&snapshot.base))
        .with_expected_actual("no permanent outbox failures", detail)
        .with_evidence(evidence)
        .with_security_exposure(SecurityExposure::OperationalMetadata)
    } else if oldest_pending_age >= 60 || pending > 0 || retrying > 0 {
        Check::warn(
            "dispatch_outbox",
            CheckGroup::Core,
            "Dispatch Outbox",
            detail.clone(),
            "Pending/retrying outbox items remain. If oldest age increases, check the delivery worker.",
        )
        .with_subsystem("health")
        .with_path(health_detail_endpoint(&snapshot.base))
        .with_expected_actual("empty or fresh outbox", detail)
        .with_evidence(evidence)
        .with_security_exposure(SecurityExposure::OperationalMetadata)
    } else {
        Check::ok(
            "dispatch_outbox",
            CheckGroup::Core,
            "Dispatch Outbox",
            detail.clone(),
        )
        .with_subsystem("health")
        .with_path(health_detail_endpoint(&snapshot.base))
        .with_expected_actual("outbox healthy", detail)
        .with_evidence(evidence)
    }
}

#[cfg(all(test, unix))]
#[path = "dispatch_outbox_tests.rs"]
mod tests;
