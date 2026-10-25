"""Restart safety: complete mailbox observations and the server the run restarts."""

from __future__ import annotations

import urllib.parse
from typing import Any

from . import assertions

TARGET_BY_PORT = {8797: "dev", 8791: "release"}


def target(options: Any, args: Any) -> str:
    """Pick the restarted server from the run (override, base-URL port); a scenario may only repeat it."""
    base_url = str(getattr(args, "base_url", "") or "")
    by_port = TARGET_BY_PORT.get(urllib.parse.urlparse(base_url).port)
    override = getattr(args, "restart_target_override", None)
    declared = (options or {}).get("target") if isinstance(options, dict) else None
    chosen = override or by_port or declared or "release"
    if chosen not in ("dev", "release") or (by_port and chosen != by_port) or (declared and not override and declared != chosen):
        raise ValueError(f"restart target {chosen!r} does not match base URL {base_url} (declared {declared!r}); "
                         "pass --restart-target-override or AGENTDESK_E2E_RESTART_TARGET")
    return chosen


def validate_binding(scenario: dict[str, Any], args: Any) -> None:
    for step in scenario.get("steps") or []:
        if isinstance(step, dict) and "restart_dcserver" in step:
            target(step["restart_dcserver"], args)


def observed_mailboxes(detail: dict[str, Any]) -> list[dict[str, Any]]:
    mailboxes = detail.get("mailboxes")
    if not isinstance(mailboxes, list):
        raise assertions.AssertionError(
            "refusing to restart dcserver: /api/health/detail mailboxes is not a list "
            f"(mailboxes={mailboxes!r})"
        )
    failures = detail.get("mailbox_observation_failures", [])
    if not isinstance(failures, list) or failures:
        raise assertions.AssertionError(
            "refusing to restart dcserver: incomplete mailbox observation: "
            f"mailbox_observation_failures={failures!r}"
        )
    providers = detail.get("providers", [])
    if not isinstance(providers, list):
        raise assertions.AssertionError(
            "refusing to restart dcserver: incomplete mailbox observation: "
            f"providers is not a list ({providers!r})"
        )
    for provider in providers:
        # Older health responses omit completeness; explicit incomplete observations are unsafe.
        if (not isinstance(provider, dict) or ("runtime_state_complete" in provider
                and provider["runtime_state_complete"] is not True)):
            raise assertions.AssertionError(
                "refusing to restart dcserver: incomplete mailbox observation: "
                f"providers entry={provider!r}"
            )
    return mailboxes
