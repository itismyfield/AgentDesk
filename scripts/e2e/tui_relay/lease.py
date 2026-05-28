"""Single-run lease file to keep two driver invocations from clashing."""

from __future__ import annotations

import contextlib
import errno
import os
import time
from pathlib import Path

DEFAULT_LEASE_DIR = Path("/tmp")
DEFAULT_LEASE_NAME = "agentdesk-e2e-relay"


def lease_path_for(cell: str | None) -> Path:
    """Per-cell lease path so different cells can run in parallel."""
    if cell:
        return DEFAULT_LEASE_DIR / f"{DEFAULT_LEASE_NAME}.{cell}.lease"
    return DEFAULT_LEASE_DIR / f"{DEFAULT_LEASE_NAME}.lease"


@contextlib.contextmanager
def acquire(
    run_id: str,
    *,
    path: Path | None = None,
    cell: str | None = None,
    ttl_s: float = 60 * 60,
):
    """Acquire an exclusive lease file. Refuses if a fresh lease already exists."""

    lease_path = path or lease_path_for(cell)
    now = time.time()
    existing = _read_lease(lease_path)
    if existing and now - existing["acquired_at"] < ttl_s:
        raise RuntimeError(
            f"existing lease at {lease_path} held by run={existing['run_id']} since "
            f"{existing['acquired_at']}; refusing to start"
        )
    try:
        fd = os.open(str(lease_path), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    except OSError as error:
        raise RuntimeError(f"cannot open lease file {lease_path}: {error}") from error
    with os.fdopen(fd, "w") as fp:
        fp.write(f"{run_id}|{now}\n")
    try:
        yield lease_path
    finally:
        with contextlib.suppress(OSError):
            os.unlink(lease_path)


def _read_lease(lease_path: Path) -> dict | None:
    try:
        text = lease_path.read_text().strip()
    except OSError as error:
        if error.errno == errno.ENOENT:
            return None
        return None
    if "|" not in text:
        return None
    run_id, acquired_at = text.split("|", 1)
    try:
        return {"run_id": run_id, "acquired_at": float(acquired_at)}
    except ValueError:
        return None
