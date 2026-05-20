"""Thin Discord wrapper around AgentDesk's /api/discord/* routes.

Uses stdlib urllib so the driver runs on a vanilla Python 3 with no pip install.
"""

from __future__ import annotations

import dataclasses
import json
import shutil
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Any


@dataclasses.dataclass(frozen=True)
class DiscordClient:
    base_url: str
    timeout_s: float = 30.0
    # When set, prompts are sent via `agentdesk send-to-agent` instead of the
    # plain `POST /api/discord/send`. send-to-agent goes through the announce
    # bot's handoff path which causes dcserver to auto-spawn the target
    # agent's tmux session for both cc and cdx providers (issue #2705). The
    # plain send endpoint records the message but does not trigger dispatch
    # for newly-active channels, which is why baseline runs starved.
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

    def send_prompt(
        self,
        channel_id: int | str,
        content: str,
        *,
        channel_kind: str,
    ) -> dict[str, Any]:
        """Send a prompt that needs to land in the target agent's TUI.

        Routes through `agentdesk send-to-agent --no-prefix` when a handoff
        identity is configured, so dcserver's dispatch path auto-spawns the
        agent's tmux session (cc *and* cdx). Falls back to `send()` otherwise.
        """

        if not self.handoff_to_agent or not self.handoff_from_agent:
            return self.send(channel_id, content)

        if channel_kind not in ("cc", "cdx"):
            raise ValueError(f"channel_kind must be 'cc' or 'cdx', got {channel_kind!r}")

        cli = shutil.which("agentdesk") or "agentdesk"
        cmd = [
            cli,
            "send-to-agent",
            "--from",
            self.handoff_from_agent,
            "--to",
            self.handoff_to_agent,
            "--message",
            content,
            "--channel-kind",
            channel_kind,
            "--no-prefix",
        ]
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=self.timeout_s)
        if proc.returncode != 0:
            raise RuntimeError(
                f"send-to-agent exit={proc.returncode}: stdout={proc.stdout!r} stderr={proc.stderr!r}"
            )
        stdout = proc.stdout.strip()
        if not stdout:
            return {}
        try:
            return json.loads(stdout)
        except json.JSONDecodeError:
            return {"raw": stdout}

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
        # `Connection: close` prevents urllib's default HTTP/1.1 keep-alive
        # from reusing the same socket for back-to-back polls. The reused
        # connection could serve a cached response and the driver would
        # observe a stale window — exactly the symptom in #2718 where the
        # response landed in the channel within seconds but the polling
        # loop kept seeing the earlier snapshot until it timed out.
        request = urllib.request.Request(
            url,
            method="GET",
            headers={"Connection": "close"},
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout_s) as response:
                payload = response.read().decode("utf-8")
        except urllib.error.HTTPError as error:
            raise RuntimeError(
                f"fetch_messages HTTP {error.code}: "
                f"{error.read().decode('utf-8', 'replace')}"
            ) from error
        except urllib.error.URLError as error:
            # Bubble up transport-level failures instead of silently
            # returning [] from a swallowed exception further up the stack.
            raise RuntimeError(f"fetch_messages URL error: {error}") from error
        body = json.loads(payload) if payload else []
        if isinstance(body, list):
            return body
        return body.get("messages", [])

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
        # never query for it again. `observed_ids` keeps duplicate handling
        # cheap so re-fetching the same window every poll is safe.
        # See #2718 driver follow-up.
        deadline = time.monotonic() + timeout_s
        observed: list[dict[str, Any]] = []
        observed_ids: set[str] = set()
        found: dict[str, Any] | None = None
        poll = 0
        while time.monotonic() < deadline and found is None:
            poll += 1
            messages = self.fetch_messages(channel_id, after_id=after_id, limit=100)
            messages = sorted(messages, key=lambda m: int(m.get("id", "0")))
            new_messages = [
                m for m in messages if str(m.get("id") or "") not in observed_ids
            ]
            if debug_enabled:
                print(
                    f"[wait_for_message] poll={poll} label={debug_label!r} "
                    f"after_id={after_id!r} fetched={len(messages)} "
                    f"new={len(new_messages)} observed_so_far={len(observed)}"
                )
            for message in new_messages:
                mid = str(message.get("id") or "")
                if mid:
                    observed.append(message)
                    observed_ids.add(mid)
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
