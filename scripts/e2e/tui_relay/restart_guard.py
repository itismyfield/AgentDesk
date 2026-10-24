"""Require complete mailbox observations before evaluating restart safety."""

from __future__ import annotations

from typing import Any

from . import assertions


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
