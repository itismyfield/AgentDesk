"""Local HTTP authentication shared by health and Discord transports."""

from urllib.parse import urlsplit


def loopback_origin(base_url):
    try:
        url = urlsplit(base_url)
        if (url.scheme not in {"http", "https"} or url.username is not None
                or url.password is not None or url.hostname not in {"127.0.0.1", "localhost", "::1"}):
            return None
        host = f"[{url.hostname}]" if url.hostname == "::1" else url.hostname
        return f"{url.scheme}://{host}" + (f":{url.port}" if url.port is not None else "")
    except ValueError:
        return None


def request_headers(base_url, headers):
    origin = loopback_origin(base_url)
    return {**headers, **({"Origin": origin} if origin else {})}
