"""Queue acknowledgements may repeat once for each referenced input."""

from typing import Any

QUEUE_NOTICE = "큐에 들어갔습니다"


def duplicate_content_key(message: dict[str, Any], body: str) -> tuple[str, str]:
    reference_id = ""
    if body == QUEUE_NOTICE:
        reference_id = str(
            (message.get("message_reference") or {}).get("message_id")
            or (message.get("referenced_message") or {}).get("id")
            or ""
        )
    return body, reference_id
