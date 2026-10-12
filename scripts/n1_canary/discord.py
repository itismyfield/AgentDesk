"""GET-only Discord collection; incomplete pages never establish absence."""
import ipaddress
import os
from http.client import HTTPException
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode, urlsplit
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener

from . import Unknown, loads
from .manifest import snowflake


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def getter(base_url, token_env):
    url = urlsplit(base_url)
    try:
        loopback = ipaddress.ip_address(url.hostname or "").is_loopback
    except ValueError:
        loopback = False
    if (url.username or url.password or url.query or url.fragment
            or not ((url.scheme == "https" and url.netloc == "discord.com" and url.path == "/api/v10")
                    or (url.scheme == "http" and loopback and url.path == "/api/v10"))):
        raise Unknown("discord_endpoint_invalid")
    token = os.environ.get(token_env or "")
    if not token and not loopback:
        raise Unknown("discord_auth_unavailable")
    opener = build_opener(ProxyHandler({}), NoRedirect())

    def get(path):
        headers = {"Authorization": f"Bot {token}"} if token else {}
        try:
            request = Request(base_url + path, headers=headers, method="GET")
            with opener.open(request, timeout=10) as response:
                return loads(response.read())
        except HTTPError as error:
            raise Unknown("discord_rate_limited" if error.code == 429 else "discord_http_error") from None
        except (OSError, ValueError, URLError, HTTPException):
            raise Unknown("discord_response_unavailable") from None
    return get


def message_valid(message, channel):
    return (isinstance(message, dict) and snowflake(message.get("id"))
            and message.get("channel_id") == str(channel) and isinstance(message.get("author"), dict)
            and snowflake(message["author"].get("id")))


def collect(scope, get):
    result = dict(complete=False, reasons=[], channels=scope["channel_ids"],
                  lower_inclusive=scope["lower_inclusive"], upper_exclusive=scope["upper_exclusive"],
                  pages=0, page_boundaries=[], messages=[], individual={})
    try:
        for channel in scope["channel_ids"]:
            cursor = int(scope["upper_exclusive"])
            for _ in range(scope["max_pages"]):
                page = get(f"/channels/{channel}/messages?" + urlencode({"limit": 100, "before": str(cursor)}))
                result["pages"] += 1
                if not isinstance(page, list) or len(page) > 100 or not all(message_valid(m, channel) for m in page):
                    raise Unknown("discord_page_invalid")
                ids = [int(m["id"]) for m in page]
                if ids != sorted(set(ids), reverse=True) or any(i >= cursor for i in ids):
                    raise Unknown("discord_page_boundary")
                result["page_boundaries"].append({"channel_id": channel, "before": str(cursor), "ids": [str(i) for i in ids]})
                result["messages"].extend(m for m in page if int(m["id"]) >= int(scope["lower_inclusive"]))
                if not ids or ids[-1] <= int(scope["lower_inclusive"]):
                    break
                cursor = ids[-1]
            else:
                raise Unknown("discord_partial_collection")
            for identity in scope["individual_ids"]:
                message = get(f"/channels/{channel}/messages/{identity}")
                if not message_valid(message, channel) or message["id"] != identity:
                    raise Unknown("discord_individual_invalid")
                result["individual"][identity] = message
        if not visibility_proven(result):
            raise Unknown("discord_visibility_unverified")
        result["complete"] = True
    except Unknown as error:
        result["reasons"].append(str(error))
    return result


def visibility_proven(value):
    # An empty page closes pagination; a successful message lookup establishes visibility.
    return bool(value.get("messages") or value.get("individual"))


def boundaries_valid(value, scope):
    pages = value.get("page_boundaries")
    if not isinstance(pages, list) or len(pages) != value.get("pages"):
        return False
    cursor, expected, closed = int(scope["upper_exclusive"]), [], False
    for page in pages:
        if (closed or not isinstance(page, dict) or set(page) != {"channel_id", "before", "ids"}
                or page["channel_id"] != scope["channel_ids"][0] or page["before"] != str(cursor)
                or not isinstance(page["ids"], list) or len(page["ids"]) > 100
                or not all(snowflake(i) for i in page["ids"])):
            return False
        ids = [int(i) for i in page["ids"]]
        if ids != sorted(set(ids), reverse=True) or any(i >= cursor for i in ids):
            return False
        expected.extend(str(i) for i in ids if i >= int(scope["lower_inclusive"]))
        closed = not ids or ids[-1] <= int(scope["lower_inclusive"])
        if ids:
            cursor = ids[-1]
    messages = value.get("messages")
    return (closed and isinstance(messages, list) and all(isinstance(m, dict) for m in messages)
            and [m.get("id") for m in messages] == expected)


def history_valid(value, scope):
    return (isinstance(value, dict) and value.get("complete") is True and value.get("reasons") == []
            and value.get("channels") == scope["channel_ids"]
            and value.get("lower_inclusive") == scope["lower_inclusive"]
            and value.get("upper_exclusive") == scope["upper_exclusive"]
            and type(value.get("pages")) is int and 0 < value["pages"] <= scope["max_pages"]
            and boundaries_valid(value, scope)
            and isinstance(value.get("messages"), list)
            and all(message_valid(m, scope["channel_ids"][0])
                    and int(scope["lower_inclusive"]) <= int(m["id"]) < int(scope["upper_exclusive"])
                    for m in value["messages"])
            and isinstance(value.get("individual"), dict) and set(value["individual"]) == set(scope["individual_ids"])
            and all(message_valid(m, scope["channel_ids"][0]) and m["id"] == identity
                    for identity, m in value["individual"].items())
            and visibility_proven(value))
