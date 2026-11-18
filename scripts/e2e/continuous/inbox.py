"""Deploy-completion requests: one secret-free file per request id, run at most once.

Only the newest pending request runs; older ones are recorded as superseded, not caught up.
"""

import re

from .store import read_json, write_json

KEYS = {"schema_version", "request_id", "deploy_sha", "submitted_at"}
REQUEST_ID = re.compile(r"[A-Za-z0-9._-]{1,128}")
SHA = re.compile(r"[0-9a-f]{40}")


def valid(row):
    return (isinstance(row, dict) and set(row) == KEYS and row["schema_version"] == 1
            and isinstance(row["request_id"], str) and REQUEST_ID.fullmatch(row["request_id"]) is not None
            and isinstance(row["deploy_sha"], str) and SHA.fullmatch(row["deploy_sha"]) is not None
            and type(row["submitted_at"]) in (int, float))


def submit(state_dir, request_id, deploy_sha, now):
    row = {"schema_version": 1, "request_id": request_id, "deploy_sha": deploy_sha, "submitted_at": now}
    if not valid(row):
        raise ValueError("deploy request needs a safe request id and a full lowercase sha")
    path = state_dir / "inbox" / f"{request_id}.json"
    if read_json(path, None) is None:
        write_json(path, row)
    return read_json(path, None)


def ledger(state_dir):
    return read_json(state_dir / "requests.json", {})


def record(state_dir, updates):
    rows = ledger(state_dir)
    rows.update(updates)
    write_json(state_dir / "requests.json", rows)


def pending(state_dir):
    """Return (newest pending request or None, older pending ids, invalid file stems)."""
    done, rows, invalid = ledger(state_dir), [], []
    for path in sorted((state_dir / "inbox").glob("*.json")):
        row = read_json(path, None)
        if not valid(row) or row["request_id"] != path.stem:
            invalid.append(path.stem)
        elif row["request_id"] not in done:
            rows.append(row)
    rows.sort(key=lambda row: (row["submitted_at"], row["request_id"]))
    newest = rows[-1] if rows else None
    return newest, [row["request_id"] for row in rows[:-1]], [stem for stem in invalid if stem not in done]
