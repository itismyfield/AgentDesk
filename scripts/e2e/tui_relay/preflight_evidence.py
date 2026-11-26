"""Bounded diagnostic fields passed from preflight to the continuous summary."""

from . import health_readiness


STAGES = ("health_read", "detail_readiness", "herdr_readiness", "herdr_status",
          "writer_not_ready", "busy", "reset_state", "preflight")
OBSERVATION_FAILURE_STAGES = {"health_read", "herdr_status", "preflight"}
STATUSES = ("healthy", "degraded", "unhealthy")
CODES = ("turn_presence_unknown", "tui_o:reader_retention_stalled", "relay_verdict_unknown", "herdr", "unclassified",
         "tui_o:binding_missing", "provider:pending_queue_depth")
SCOPES = ("target", "foreign", "unknown")


def bounded(value):
    if not isinstance(value, dict) or value.get("stage") not in STAGES:
        return None
    result = {"stage": value["stage"]}
    http = value.get("http_status")
    if type(http) is int and 100 <= http <= 599:
        result["http_status"] = http
    if value.get("status") in STATUSES:
        result["status"] = value["status"]
    if type(value.get("fully_recovered")) is bool:
        result["fully_recovered"] = value["fully_recovered"]
    reasons = value.get("reasons")
    if isinstance(reasons, list):
        pairs = {(reason["code"], reason["scope"]) for reason in reasons
                 if isinstance(reason, dict) and reason.get("code") in CODES and reason.get("scope") in SCOPES}
        pairs = {(code, scope) for code, scope in pairs if code not in ("herdr", "unclassified") or scope == "unknown"}
        result["reasons"] = [{"code": code, "scope": scope} for code, scope in sorted(pairs)]
    return result


def capture(stage, detail, *, channel, http_status=None):
    detail = detail if isinstance(detail, dict) else {}
    reasons = None
    if isinstance(detail.get("degraded_reasons"), list):
        reasons = [{"code": code, "scope": scope}
                   for _, code, scope in health_readiness.classify_degraded_reasons(detail, channel)]
    return bounded({"stage": stage, "status": detail.get("status"), "fully_recovered": detail.get("fully_recovered"),
                    "http_status": http_status, "reasons": reasons})
