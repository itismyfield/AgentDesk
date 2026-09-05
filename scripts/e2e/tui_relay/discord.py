"""Thin Discord wrapper around AgentDesk's /api/discord/* routes.

Uses stdlib urllib so the driver runs on a vanilla Python 3 with no pip install.
"""

from __future__ import annotations

import dataclasses
import json
import math
import time
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Mapping
from email.utils import parsedate_to_datetime
from typing import Any


MESSAGE_FETCH_MAX_ATTEMPTS = 3
MESSAGE_FETCH_RETRY_BUDGET_S = 15.0
MESSAGE_FETCH_MIN_RETRY_DELAY_S = 0.1
MESSAGE_FETCH_BODY_LIMIT = 512


def _fetch_error(status: int | str, payload: str, reason: str) -> RuntimeError:
    excerpt = repr(payload[:MESSAGE_FETCH_BODY_LIMIT])[:MESSAGE_FETCH_BODY_LIMIT]
    if len(payload) > MESSAGE_FETCH_BODY_LIMIT:
        excerpt += "..."
    return RuntimeError(f"fetch_messages observed HTTP {status}: {reason}; body={excerpt}")


def _fetch_retry_delay(headers, body: Any) -> float:
    values = [
        (name, headers.get(name))
        for name in ("X-RateLimit-Reset-After", "Retry-After")
        if headers.get(name) is not None
    ]
    if isinstance(body, dict) and "retry_after" in body:
        values.append(("retry_after", body["retry_after"]))
    if not values:
        raise ValueError("missing rate-limit delay")
    delays = []
    for name, value in values:
        try:
            if isinstance(value, bool) or not isinstance(value, (int, float, str)):
                raise ValueError
            try:
                delay = float(value)
            except ValueError:
                if name != "Retry-After":
                    raise
                reset_at = parsedate_to_datetime(value)
                if reset_at.tzinfo is None:
                    raise ValueError
                delay = max(0.0, reset_at.timestamp() - time.time())
            if not math.isfinite(delay) or delay < 0:
                raise ValueError
        except (TypeError, ValueError, OverflowError):
            raise ValueError(f"invalid {name} delay") from None
        delays.append(delay)
    # Honor every supplied delay, including a longer bucket reset. The floor
    # prevents repeated zero-delay responses from causing a tight retry loop.
    return max(MESSAGE_FETCH_MIN_RETRY_DELAY_S, *delays)


@dataclasses.dataclass(frozen=True)
class DiscordClient:
    base_url: str
    timeout_s: float = 30.0
    # When set, prompts are sent via `agentdesk send-to-agent` instead of the
    # plain `POST /api/discord/send`. The direct headless turn API starts the
    # worker even when the provider bot is not currently watching the channel.
    handoff_to_agent: str | None = None
    handoff_from_agent: str | None = None

    def send(self, channel_id: int | str, content: str) -> dict[str, Any]:
        body = json.dumps({"channel_id": str(channel_id), "content": content}).encode("utf-8")
        request = urllib.request.Request(
            f"{self.base_url}/api/discord/send",
            data=body,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        with urllib.request.urlopen(request, timeout=self.timeout_s) as response:
            payload = response.read().decode("utf-8")
        if not payload:
            return {}
        return json.loads(payload)

    def send_control(self, channel_id: int | str, content: str) -> dict[str, Any]:
        """Send harness bookkeeping with notify-bot so workers do not wake.

        The E2E setup/teardown markers are for the operator-visible transcript
        and for assertion window boundaries. Sending them through the announce
        bot can wake the worker agent and recursively start another E2E run.
        """

        body = json.dumps(
            {
                "target": f"channel:{channel_id}",
                "content": content,
                "source": "adk-e2e-orchestrator",
                "bot": "notify",
            }
        ).encode("utf-8")
        request = urllib.request.Request(
            f"{self.base_url}/api/discord/send",
            data=body,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        with urllib.request.urlopen(request, timeout=self.timeout_s) as response:
            payload = response.read().decode("utf-8")
        if not payload:
            return {}
        return json.loads(payload)

    def delete_message(
        self,
        channel_id: int | str,
        message_id: int | str,
        *,
        provider: str,
    ) -> dict[str, Any]:
        body = json.dumps({"provider": provider}).encode("utf-8")
        request = urllib.request.Request(
            f"{self.base_url}/api/e2e/discord/channels/{channel_id}/messages/{message_id}",
            data=body,
            headers={"Content-Type": "application/json"},
            method="DELETE",
        )
        return self._read_json(request, operation="delete_message")

    def inject_failure(
        self,
        channel_id: int | str,
        *,
        provider: str,
        operation: str,
        count: int = 1,
    ) -> dict[str, Any]:
        body = json.dumps(
            {
                "provider": provider,
                "channel_id": str(channel_id),
                "operation": operation,
                "count": count,
            }
        ).encode("utf-8")
        request = urllib.request.Request(
            f"{self.base_url}/api/e2e/discord/failures",
            data=body,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        return self._read_json(request, operation="inject_failure")

    def clear_failure(
        self,
        channel_id: int | str,
        *,
        provider: str,
        operation: str,
    ) -> dict[str, Any]:
        body = json.dumps(
            {
                "provider": provider,
                "channel_id": str(channel_id),
                "operation": operation,
            }
        ).encode("utf-8")
        request = urllib.request.Request(
            f"{self.base_url}/api/e2e/discord/failures",
            data=body,
            headers={"Content-Type": "application/json"},
            method="DELETE",
        )
        return self._read_json(request, operation="clear_failure")

    def _read_json(
        self, request: urllib.request.Request, *, operation: str
    ) -> dict[str, Any]:
        try:
            with urllib.request.urlopen(request, timeout=self.timeout_s) as response:
                payload = response.read().decode("utf-8")
        except urllib.error.HTTPError as error:
            detail = error.read().decode("utf-8", "replace")
            raise RuntimeError(f"{operation} HTTP {error.code}: {detail}") from error
        except urllib.error.URLError as error:
            raise RuntimeError(f"{operation} URL error: {error}") from error
        if not payload:
            return {}
        parsed = json.loads(payload)
        if not isinstance(parsed, dict):
            raise RuntimeError(f"{operation} returned non-object JSON: {parsed!r}")
        return parsed

    def send_prompt(
        self,
        channel_id: int | str,
        content: str,
        *,
        channel_kind: str = "cc",
    ) -> dict[str, Any]:
        """Send a prompt that needs to land in the target agent's TUI.

        Routes through `agentdesk send-to-agent --no-prefix` when a handoff
        identity is configured. Each E2E worker agent owns a single channel
        bound as `cc`, so `channel_kind` defaults to `cc`.
        """

        if not self.handoff_to_agent or not self.handoff_from_agent:
            return self.send(channel_id, content)

        if channel_kind not in ("cc", "cdx"):
            raise ValueError(f"channel_kind must be 'cc' or 'cdx', got {channel_kind!r}")

        provider = "codex" if channel_kind == "cdx" else "claude"
        body = json.dumps(
            {
                "prompt": content,
                "source": self.handoff_from_agent,
                "provider": provider,
                "channel_id": str(channel_id),
                "metadata": {"e2e_handoff_from": self.handoff_from_agent},
            }
        ).encode("utf-8")
        target = urllib.parse.quote(self.handoff_to_agent, safe="")
        url = f"{self.base_url}/api/agents/{target}/turn/start"
        deadline = time.monotonic() + self.timeout_s
        while True:
            request = urllib.request.Request(
                url,
                data=body,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            try:
                with urllib.request.urlopen(request, timeout=min(10.0, self.timeout_s)) as response:
                    payload = response.read().decode("utf-8")
                break
            except urllib.error.HTTPError as error:
                error_body = error.read().decode("utf-8", "replace")
                if (
                    error.code == 409
                    and "mailbox is busy" in error_body
                    and time.monotonic() < deadline
                ):
                    time.sleep(1.0)
                    continue
                raise RuntimeError(f"turn/start HTTP {error.code}: {error_body}") from error
        if not payload:
            return {}
        return json.loads(payload)

    def fetch_messages(
        self,
        channel_id: int | str,
        *,
        limit: int = 50,
        after_id: str | None = None,
    ) -> list[dict[str, Any]]:
        params: dict[str, Any] = {"limit": limit}
        if after_id:
            params["after"] = after_id
        query = urllib.parse.urlencode(params)
        url = f"{self.base_url}/api/discord/channels/{channel_id}/messages?{query}"
        # Preserve the existing explicit connection-close request for polls.
        request = urllib.request.Request(
            url,
            method="GET",
            headers={"Connection": "close"},
        )
        deadline = time.monotonic() + MESSAGE_FETCH_RETRY_BUDGET_S
        waited = 0.0
        status: int | str = "unavailable"
        payload = ""
        for attempt in range(1, MESSAGE_FETCH_MAX_ATTEMPTS + 1):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise _fetch_error(status, payload, "retry budget exhausted")
            try:
                with urllib.request.urlopen(request, timeout=min(self.timeout_s, remaining)) as response:
                    status, headers, raw = response.status, response.headers, response.read()
            except urllib.error.HTTPError as error:
                with error:
                    status, headers, raw = error.code, error.headers, error.read()
            except urllib.error.URLError as error:
                raise RuntimeError(f"fetch_messages URL error: {error}") from error
            payload = raw.decode("utf-8", "replace")
            try:
                body = json.loads(raw)
            except ValueError:
                if status != 429:
                    raise _fetch_error(status, payload, "invalid JSON response") from None
                # An observed 429 may provide its delay only in HTTP headers.
                body = None

            messages = body.get("messages") if isinstance(body, dict) else body
            # The current API can hide an upstream error inside HTTP 200.
            # Infer rate limiting only from structured retry metadata, not text.
            wrapped_rate_limit = (
                status == 200 and isinstance(messages, dict) and "retry_after" in messages
            )
            if status != 429 and not wrapped_rate_limit:
                if not 200 <= status < 300:
                    raise _fetch_error(status, payload, "unsuccessful response")
                if not isinstance(messages, list):
                    raise _fetch_error(status, payload, "expected message array or messages envelope")
                if not all(isinstance(message, Mapping) for message in messages):
                    raise _fetch_error(status, payload, "message array contains a non-object element")
                return messages

            reason = "rate limit inferred from wrapped retry_after" if wrapped_rate_limit else "rate limit"
            try:
                delay = _fetch_retry_delay(headers, messages if wrapped_rate_limit else body)
            except ValueError as error:
                raise _fetch_error(status, payload, f"{reason}: {error}") from None
            if attempt == MESSAGE_FETCH_MAX_ATTEMPTS:
                raise _fetch_error(status, payload, f"{reason}: attempt limit exhausted")
            # Both elapsed request time and cumulative waits consume the budget.
            # Socket timeouts above are capped too; urllib has no total-read deadline.
            remaining = min(deadline - time.monotonic(), MESSAGE_FETCH_RETRY_BUDGET_S - waited)
            if delay >= remaining:
                raise _fetch_error(status, payload, f"{reason}: retry delay exceeds remaining budget")
            time.sleep(delay)
            waited += delay

    def wait_for_message(
        self,
        channel_id: int | str,
        *,
        predicate,
        after_id: str | None = None,
        timeout_s: float = 120.0,
        poll_interval_s: float = 5.0,
        debug_label: str | None = None,
    ) -> tuple[dict[str, Any] | None, list[dict[str, Any]]]:
        """Poll until *predicate* returns truthy.

        Returns ``(found, observed)`` where *observed* is every message
        encountered during polling (in arrival order). The caller is expected
        to feed *observed* into its assertion window so we never lose
        duplicate / chrome signal that occurred while we were waiting for the
        target match.

        Set ``AGENTDESK_E2E_WAIT_DEBUG=1`` to print one line per poll with
        the running fetch counts and last_id progression — useful when
        debugging \"channel had it but driver never saw it\" regressions.
        """

        import os as _os

        debug_enabled = bool(_os.environ.get("AGENTDESK_E2E_WAIT_DEBUG"))
        # Keep the *caller-supplied* `after_id` fixed across polls instead of
        # advancing it to the max fetched id. Discord / dcserver can return
        # a partial snapshot under race — the assistant reply marker may
        # not yet be visible on the first poll right after a send_prompt,
        # only the user message that arrived after the marker can show up
        # later — and once `last_id` slides past the marker's id we would
        # never query for it again. `observed_by_id` tracks seen values while
        # still allowing same-ID edits to be observed in the fixed window.
        # See #2718 driver follow-up.
        deadline = time.monotonic() + timeout_s
        observed: list[dict[str, Any]] = []
        observed_by_id: dict[str, dict[str, Any]] = {}
        found: dict[str, Any] | None = None
        poll = 0
        while time.monotonic() < deadline and found is None:
            poll += 1
            messages = self.fetch_messages(channel_id, after_id=after_id, limit=100)
            messages = sorted(messages, key=lambda m: int(m.get("id", "0")))
            new_or_updated_messages: list[dict[str, Any]] = []
            for message in messages:
                mid = str(message.get("id") or "")
                if not mid:
                    new_or_updated_messages.append(message)
                    continue
                previous = observed_by_id.get(mid)
                if previous is None or _message_changed(previous, message):
                    observed_by_id[mid] = message
                    new_or_updated_messages.append(message)
            if debug_enabled:
                print(
                    f"[wait_for_message] poll={poll} label={debug_label!r} "
                    f"after_id={after_id!r} fetched={len(messages)} "
                    f"new_or_updated={len(new_or_updated_messages)} "
                    f"observed_so_far={len(observed)}"
                )
            for message in new_or_updated_messages:
                mid = str(message.get("id") or "")
                if mid:
                    observed.append(message)
                if predicate(message):
                    found = message
                    break
            if found is None:
                time.sleep(poll_interval_s)
        if debug_enabled and found is None:
            print(
                f"[wait_for_message] timeout after {poll} polls "
                f"label={debug_label!r} after_id={after_id!r} "
                f"observed_total={len(observed)}"
            )
        return found, observed


def _message_changed(old: dict[str, Any], new: dict[str, Any]) -> bool:
    return (old.get("content") or "") != (new.get("content") or "") or old.get(
        "edited_timestamp"
    ) != new.get("edited_timestamp")
