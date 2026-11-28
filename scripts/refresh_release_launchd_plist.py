#!/usr/bin/env python3
"""Read or refresh a release service plist."""
from __future__ import annotations

import argparse
import os
from pathlib import Path
import plistlib
import stat
import subprocess
import sys
import tempfile
from xml.parsers.expat import ExpatError


def read_plist(path: Path) -> dict:
    with path.open("rb") as stream:
        data = plistlib.load(stream)
    if not isinstance(data, dict):
        raise ValueError("launchd plist must be a dictionary")
    return data


def exec_role_context(path: Path, label: str, command: list[str]) -> None:
    try:
        plist = read_plist(path)
        cwd = Path(plist["WorkingDirectory"])
        env = plist["EnvironmentVariables"]
        if plist.get("Label") != label or not cwd.is_absolute() or not isinstance(env, dict):
            raise ValueError("invalid service context")
        if not all(isinstance(key, str) and isinstance(value, str) for key, value in env.items()):
            raise ValueError("invalid service environment")
        # Leave an absent PATH detectable after the shell applies the deployment overrides.
        env.setdefault("PATH", "")
        os.chdir(cwd)
        os.execve(command[0], command, env)
    except (OSError, ValueError, TypeError, KeyError, ExpatError):
        raise SystemExit("Cannot verify the existing release plist environment or working directory.")


def refresh(binary: Path, home: Path, root: Path) -> None:
    destination = home / "Library/LaunchAgents/com.agentdesk.release.plist"
    destination.parent.mkdir(parents=True, exist_ok=True)
    logs = {}
    mode = 0o600
    if destination.exists():
        current = read_plist(destination)
        mode = stat.S_IMODE(destination.stat().st_mode)
        for key in ("StandardOutPath", "StandardErrorPath"):
            if key in current:
                value = current[key]
                if not isinstance(value, str) or not Path(value).is_absolute():
                    raise ValueError(f"existing launchd {key} must be an absolute path")
                logs[key] = value

    # Generate and validate away from the live plist. A failed command or an
    # invalid candidate must leave the operator's existing service intact.
    with tempfile.TemporaryDirectory(prefix=".agentdesk-plist-", dir=destination.parent) as scratch:
        candidate = Path(scratch) / destination.name
        subprocess.run([
            str(binary), "emit-launchd-plist", "--flavor", "release",
            "--home", str(home), "--root-dir", str(root),
            "--agentdesk-bin", str(binary), "--output", str(candidate),
        ], check=True)
        generated = read_plist(candidate)
        if generated.get("Label") != "com.agentdesk.release":
            raise ValueError("generated launchd plist has an unexpected label")
        generated.update(logs)
        with candidate.open("wb") as stream:
            plistlib.dump(generated, stream)
            stream.flush()
            os.fsync(stream.fileno())
        candidate.chmod(mode)
        os.replace(candidate, destination)


if __name__ == "__main__":
    if sys.argv[1:2] == ["--role-context"]:
        exec_role_context(Path(sys.argv[2]), sys.argv[3], sys.argv[4:])
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--home", required=True, type=Path)
    parser.add_argument("--root-dir", required=True, type=Path)
    args = parser.parse_args()
    refresh(args.binary, args.home, args.root_dir)
