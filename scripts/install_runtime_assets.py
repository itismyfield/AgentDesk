#!/usr/bin/env python3
"""Install release-owned assets without replacing operator-owned files."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import sys
import tempfile


def runtime_assets(root: Path) -> tuple[str, ...]:
    assets = tuple(line.strip() for line in
                   (root / "scripts/runtime-assets.txt").read_text(encoding="utf-8").splitlines()
                   if line.strip() and not line.lstrip().startswith("#"))
    for asset in assets:
        path = PurePosixPath(asset)
        if path.is_absolute() or ".." in path.parts or "\\" in asset or str(path) != asset or asset == ".":
            raise ValueError(f"unsafe runtime asset: {asset}")
    return assets


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def linked_path(root: Path, relative: Path) -> bool:
    return root.is_symlink() or any((root / parent).is_symlink()
                                    for parent in (relative, *relative.parents))


def asset_files(root: Path) -> list[str]:
    assets = runtime_assets(root)
    if (root / ".git").exists():
        tracked = subprocess.check_output(["git", "-C", str(root), "ls-files", "-z", "--", *assets])
        files = [name for name in tracked.decode("utf-8").split("\0") if name]
    else:
        files = []
        for asset in assets:
            path = root / asset
            if path.is_dir():
                files.extend(p.relative_to(root).as_posix() for p in path.rglob("*") if p.is_file())
            elif path.exists() or path.is_symlink():
                files.append(asset)
    for name in files:
        if linked_path(root, Path(name)) or not (root / name).is_file():
            raise ValueError(f"runtime asset is linked or not a regular file: {name}")
    return sorted(set(files))


def atomic_copy(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    descriptor, name = tempfile.mkstemp(prefix=".asset-", dir=destination.parent)
    os.close(descriptor)
    temporary = Path(name)
    try:
        shutil.copyfile(source, temporary)
        temporary.chmod(0o755 if source.suffix == ".sh" or os.access(source, os.X_OK) else 0o644)
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)


def install(source: Path, destination: Path) -> None:
    source = source.resolve()
    destination = destination.absolute()
    ledger_relative = Path("runtime/install-assets.json")
    if linked_path(destination, ledger_relative):
        raise ValueError("runtime asset ledger is linked; refusing to write through it")
    ledger = destination / ledger_relative
    prior = json.loads(ledger.read_text(encoding="utf-8")) if ledger.exists() else {}
    if not isinstance(prior, dict) or any(not isinstance(k, str) or not isinstance(v, str)
                                         for k, v in prior.items()):
        raise ValueError("invalid runtime asset ledger")
    managed = dict(prior)
    files = asset_files(source)
    for relative in files:
        target = destination / relative
        if linked_path(destination, Path(relative)) or (target.exists() and not target.is_file()):
            print(f"Preserved operator asset: {relative}")
            continue
        if target.exists() and sha256(target) != prior.get(relative):
            print(f"Preserved operator asset: {relative}")
            continue
        atomic_copy(source / relative, target)
        managed[relative] = sha256(target)
    ledger.parent.mkdir(parents=True, exist_ok=True)
    descriptor, name = tempfile.mkstemp(prefix=".install-assets-", dir=ledger.parent)
    temporary = Path(name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
            json.dump(managed, stream, indent=2, sort_keys=True)
            stream.write("\n")
        os.replace(temporary, ledger)
    finally:
        temporary.unlink(missing_ok=True)
    print(f"Installed runtime asset contract: {len(files)} files")


if __name__ == "__main__":
    try:
        if len(sys.argv) != 3:
            raise ValueError("expected source and installation directories")
        install(Path(sys.argv[1]), Path(sys.argv[2]))
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f"Runtime asset installation failed: {error}")
