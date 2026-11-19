#!/usr/bin/env python3
"""Bind a clean release build to the exact bytes shipped beside its source record."""

from __future__ import annotations

import argparse
from collections.abc import Mapping
import contextlib
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

SUFFIX = ".source.json"
SOURCE = re.compile(r"[a-f0-9]{40}")
TARGET = re.compile(r"[A-Za-z0-9_.-]{1,80}")
MAX_RECORD = 4096


def sidecar(binary: Path) -> Path:
    return binary.with_name(binary.name + SUFFIX)


def digest(binary: Path) -> str:
    with binary.open("rb") as stream:
        checksum = hashlib.sha256()
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            checksum.update(chunk)
        return checksum.hexdigest()


def record(binary: Path, source: str, target: str) -> dict:
    if not SOURCE.fullmatch(source) or not TARGET.fullmatch(target):
        raise ValueError("invalid release source identity")
    return dict(schema_version=1, source_sha=source, target=target, sha256=digest(binary))


def verified(metadata: Path, binary: Path, *, peer: bool = False) -> dict | None:
    try:
        if metadata.is_symlink() or not metadata.is_file():
            return None
        with metadata.open("rb") as stream:
            raw = stream.read(MAX_RECORD + 1)
        if len(raw) > MAX_RECORD:
            return None
        data = json.loads(raw)
        if peer:
            source = data["source_sha"]
            data = data["running_source"]
            if data["source_sha"] != source:
                return None
        expected = record(binary, data["source_sha"], data["target"])
        return expected if data == expected and type(data["schema_version"]) is int else None
    except (OSError, ValueError, KeyError, TypeError):
        return None


def write(path: Path, data: dict) -> None:
    # Stage and publish in the same directory so readers see a complete record.
    fd, name = tempfile.mkstemp(dir=path.parent, prefix=".release-source-")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(data, stream, sort_keys=True)
            stream.write("\n")
        os.replace(name, path)
    finally:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(name)


def _release_checkout_sha(env: Mapping[str, str]) -> str:
    root = os.getcwd()
    git_env = {**env, "GIT_OPTIONAL_LOCKS": "0", "GIT_NO_LAZY_FETCH": "1",
               "GIT_NO_REPLACE_OBJECTS": "1"}
    for name in ("GIT_DIR", "GIT_COMMON_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"):
        git_env.pop(name, None)

    def git(*args: str) -> bytes:
        return subprocess.run(
            ["git", "-C", root, "-c", "core.fsmonitor=false", "-c", "core.trustctime=true",
             "-c", "core.checkStat=default", *args],
            env=git_env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            check=True, timeout=5,
        ).stdout

    try:
        top = os.fsdecode(git("rev-parse", "--show-toplevel").rstrip(b"\r\n"))
        if not top or not os.path.samefile(top, root):
            return ""
        sha = git("rev-parse", "--verify", "HEAD^{commit}").rstrip(b"\r\n")
        if not re.fullmatch(b"[0-9a-f]{40}", sha):
            return ""
        if git("status", "--porcelain=v1", "--untracked-files=all", "--ignore-submodules=none"):
            return ""
        entries = git("ls-files", "--stage", "-v", "-z").split(b"\0")
        if len(entries) < 2 or entries[-1] or not all(
                entry.startswith((b"H 100644 ", b"H 100755 ")) for entry in entries[:-1]):
            return ""
        if git("rev-parse", "--verify", "HEAD^{commit}").rstrip(b"\r\n") == sha:
            return sha.decode("ascii")
    except (OSError, subprocess.SubprocessError):
        pass
    return ""


class BuildWitness:
    def __init__(self, binary: str, metadata: str, target: str):
        self.binary = Path(binary)
        self.metadata = Path(metadata)
        self.target = target
        self.env: Mapping[str, str] = {}
        self.source = ""

    def begin(self, env: Mapping[str, str]) -> None:
        self.metadata.unlink(missing_ok=True)
        self.env = env
        self.source = _release_checkout_sha(env)

    def finish(self) -> None:
        if self.source:
            try:
                data = record(self.binary, self.source, self.target)
            except (OSError, ValueError):
                return
            if _release_checkout_sha(self.env) == self.source:
                write(self.metadata, data)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="operation", required=True)
    inspect = sub.add_parser("inspect")
    inspect.add_argument("metadata", type=Path)
    inspect.add_argument("binary", type=Path)
    inspect.add_argument("--peer", action="store_true")
    seal = sub.add_parser("seal")
    seal.add_argument("binary", type=Path)
    seal.add_argument("source")
    seal.add_argument("target")
    args = parser.parse_args()
    if args.operation == "inspect":
        data = verified(args.metadata, args.binary, peer=args.peer)
        if data is not None:
            print(data["source_sha"], data["target"])
    else:
        write(sidecar(args.binary), record(args.binary, args.source, args.target))


if __name__ == "__main__":
    main()
