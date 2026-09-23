#!/usr/bin/env python3
"""Refresh release launchd arguments while preserving operator log destinations."""
from __future__ import annotations

import argparse
import os
from pathlib import Path
import plistlib
import stat
import subprocess
import tempfile


def refresh(binary: Path, home: Path, root: Path) -> None:
    destination = home / "Library/LaunchAgents/com.agentdesk.release.plist"
    destination.parent.mkdir(parents=True, exist_ok=True)
    logs = {}
    mode = 0o600
    if destination.exists():
        with destination.open("rb") as stream:
            current = plistlib.load(stream)
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
        with candidate.open("rb") as stream:
            generated = plistlib.load(stream)
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
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--home", required=True, type=Path)
    parser.add_argument("--root-dir", required=True, type=Path)
    args = parser.parse_args()
    refresh(args.binary, args.home, args.root_dir)
