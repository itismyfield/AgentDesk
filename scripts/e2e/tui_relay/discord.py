"""Thin Discord wrapper around AgentDesk's /api/discord/* routes.

Uses stdlib urllib so the driver runs on a vanilla Python 3 with no pip install.
"""

from __future__ import annotations

import dataclasses
import json
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Any


@dataclasses.dataclass(frozen=True)
class DiscordClient:
    base_url: str
    timeout_s: float = 30.0

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
        request = urllib.request.Request(url, method="GET")
        try:
            with urllib.request.urlopen(request, timeout=self.timeout_s) as response:
                payload = response.read().decode("utf-8")
        except urllib.error.HTTPError as error:
            raise RuntimeError(f"fetch_messages HTTP {error.code}: {error.read().decode('utf-8', 'replace')}") from error
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
    ) -> dict[str, Any] | None:
        deadline = time.monotonic() + timeout_s
        last_id = after_id
        while time.monotonic() < deadline:
            messages = self.fetch_messages(channel_id, after_id=last_id)
            messages = sorted(messages, key=lambda m: int(m.get("id", "0")))
            for message in messages:
                if predicate(message):
                    return message
                last_id = message.get("id") or last_id
            time.sleep(poll_interval_s)
        return None
