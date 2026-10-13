"""Feature evidence and additive report accounting for the relay harness."""

import os
import re


SCHEMA_VERSION = 2
SCENARIO_STATUSES = ("pass", "fail", "skipped", "not_applicable", "known_gap", "unexpected_pass", "dry_run")
ASSERTION_STATUSES = ("pass", "fail", "skipped", "unevaluable", "not_applicable")


def scenario_features(scenario):
    required = set(scenario.get("requires_features") or [])
    required.update(spec["requires_feature"] for spec in scenario.get("assertions") or []
                    if isinstance(spec, dict) and "requires_feature" in spec)
    if not required:
        return frozenset()
    raw = os.environ.get("AGENTDESK_E2E_FEATURES")
    if raw is None:
        raise ValueError("AGENTDESK_E2E_FEATURES is missing; explicitly set an empty list for disabled features")
    if not raw.strip():
        return frozenset()
    features = [part.strip() for part in raw.split(",")]
    if any(not re.fullmatch(r"[a-zA-Z0-9_-]+", part) for part in features):
        raise ValueError("AGENTDESK_E2E_FEATURES is not a comma-separated feature list")
    return frozenset(features)


def skipped_assertion(feature):
    return {"status": "skipped", "passed": None, "required_feature": feature,
            "reason": f"feature explicitly disabled: {feature}"}


def annotate(result):
    counts = dict.fromkeys(ASSERTION_STATUSES, 0)
    for row in result.get("assertions") or []:
        status = row.get("status")
        if status is None:
            status = "pass" if row.get("passed") is True else "fail" if row.get("passed") is False else None
        if status in counts:
            counts[status] += 1
    counts["executed"] = counts["pass"] + counts["fail"]
    result["assertion_totals"] = counts
    result["assertion_coverage"] = (
        "partial" if result["status"] == "fail" or any(counts[s] for s in ("skipped", "unevaluable", "not_applicable"))
        else "complete" if counts["executed"] else "none"
    )
    if (result.get("failure_attribution") or {}).get("classification") == "unevaluable":
        result["execution_status"] = "unevaluable"
    elif result.get("execution_status") != "dormant":
        executed = result.get("executed", counts["executed"] > 0 or result.get("real_provider_contacted") is True)
        result["execution_status"] = "executed" if executed else result["status"]
    return result


def totals(results):
    for result in results:
        annotate(result)
    counts = {status: sum(r["status"] == status for r in results) for status in SCENARIO_STATUSES}
    counts.update({status: sum(r["execution_status"] == status for r in results)
                   for status in ("executed", "dormant", "unevaluable")})
    counts["skipped_assertions"] = sum(r["assertion_totals"]["skipped"] for r in results)
    return counts
