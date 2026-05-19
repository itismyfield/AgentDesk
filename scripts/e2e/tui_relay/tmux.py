"""tmux helpers — pane discovery, send-keys, kill. Read-only by default."""

from __future__ import annotations

import dataclasses
import shutil
import subprocess


@dataclasses.dataclass(frozen=True)
class PaneInfo:
    pane_id: str
    pid: int
    cwd: str
    session_name: str


def _have_tmux() -> bool:
    return shutil.which("tmux") is not None


def list_panes(session_name: str) -> list[PaneInfo]:
    if not _have_tmux():
        return []
    cmd = [
        "tmux",
        "list-panes",
        "-t",
        session_name,
        "-F",
        "#{pane_id}|#{pane_pid}|#{pane_current_path}|#{session_name}",
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        return []
    panes: list[PaneInfo] = []
    for line in proc.stdout.strip().splitlines():
        parts = line.split("|", 3)
        if len(parts) != 4:
            continue
        try:
            pid = int(parts[1])
        except ValueError:
            continue
        panes.append(
            PaneInfo(pane_id=parts[0], pid=pid, cwd=parts[2], session_name=parts[3])
        )
    return panes


def send_keys(session_name: str, *keys: str) -> bool:
    if not _have_tmux():
        return False
    cmd = ["tmux", "send-keys", "-t", session_name, *keys]
    proc = subprocess.run(cmd, capture_output=True, check=False)
    return proc.returncode == 0


def capture_pane(session_name: str, scroll_back: int = -200) -> str:
    if not _have_tmux():
        return ""
    cmd = ["tmux", "capture-pane", "-t", session_name, "-p", "-S", str(scroll_back)]
    proc = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        return ""
    return proc.stdout


def kill_pane(pane_id: str) -> bool:
    """Caller is responsible for re-verifying pane_id immediately before this call."""

    if not _have_tmux():
        return False
    proc = subprocess.run(
        ["tmux", "kill-pane", "-t", pane_id],
        capture_output=True,
        check=False,
    )
    return proc.returncode == 0
