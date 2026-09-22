#!/usr/bin/env python3
"""Package one native build in the shared leader/worker layout (Python >= 3.11).

Inputs are the binary, built dashboard and explicitly selected tracked assets.
Operator configuration, credentials and workspaces are never packaged.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import tarfile
import tempfile
import tomllib
import zipfile


RUNTIME_ASSETS = (
    "policies", "routines", "skills", "scripts/launchd-migrated",
    "scripts/_defaults.sh", "scripts/queue-stability-batch.sh",
    "scripts/install-windows-runtime-task.ps1", "scripts/agentdesk-dcserver.service",
    "scripts/install-windows-worker-firewall.ps1",
    "defaults.json", "agentdesk.example.yaml", "LICENSE",
)
TARGETS = {
    "aarch64-apple-darwin": ("darwin", "aarch64"),
    "x86_64-apple-darwin": ("darwin", "x86_64"),
    "x86_64-pc-windows-msvc": ("windows", "x86_64"),
    "aarch64-pc-windows-msvc": ("windows", "aarch64"),
    "x86_64-unknown-linux-gnu": ("linux", "x86_64"),
    "aarch64-unknown-linux-gnu": ("linux", "aarch64"),
}


def git(root: Path, *args: str) -> str:
    return subprocess.check_output(["git", "-C", str(root), *args]).decode("utf-8")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_binary(path: Path, target: str) -> None:
    """Reject a stale binary from another OS/CPU before publishing its name."""
    system, arch = TARGETS[target]
    with path.open("rb") as stream:
        header = stream.read(64)
        if system == "windows" and header[:2] == b"MZ" and len(header) == 64:
            stream.seek(struct.unpack_from("<I", header, 60)[0])
            pe = stream.read(6)
            expected = 0x8664 if arch == "x86_64" else 0xAA64
            valid = len(pe) == 6 and pe[:4] == b"PE\0\0" and struct.unpack_from("<H", pe, 4)[0] == expected
        elif system == "linux" and header[:6] == b"\x7fELF\x02\x01" and len(header) >= 20:
            valid = struct.unpack_from("<H", header, 18)[0] == (62 if arch == "x86_64" else 183)
        elif system == "darwin" and header[:4] == b"\xcf\xfa\xed\xfe" and len(header) >= 8:
            valid = struct.unpack_from("<I", header, 4)[0] == (0x01000007 if arch == "x86_64" else 0x0100000C)
        else:
            valid = False
    if not valid:
        raise ValueError(f"binary format/architecture does not match {target}: {path.name}")


def copy_file(source: Path, destination: Path, boundary: Path) -> None:
    # Resolve parent links too; never dereference a link outside the source tree.
    if source.is_symlink() or not source.is_file() or not source.resolve().is_relative_to(boundary.resolve()):
        raise ValueError(f"release asset is missing, linked or outside its source tree: {source}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    destination.chmod(0o755 if source.suffix == ".sh" or os.access(source, os.X_OK) else 0o644)


def write_json(path: Path, data: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8", newline="\n")


def package(root: Path, binary: Path, target: str, output: Path, dashboard: bool = True, profile: str = "release") -> Path:
    if profile not in {"release", "release-fast"}:
        raise ValueError("unsupported build profile")
    system, arch = TARGETS[target]
    verify_binary(binary, target)
    if dashboard and not (root / "dashboard/dist/index.html").is_file():
        raise ValueError("dashboard/dist/index.html is missing; build it or explicitly use --without-dashboard")
    version = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
    head = git(root, "rev-parse", "HEAD").strip()
    dirty = bool(git(root, "status", "--porcelain", "--untracked-files=normal").strip())
    migrations = sorted((root / "migrations/postgres").glob("*.sql"))
    generated = datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")
    name = f"agentdesk-{system}-{arch}"
    filename = f"{name}.{'zip' if system == 'windows' else 'tar.gz'}"
    output.mkdir(parents=True, exist_ok=True)
    # Cleanup owns only this fresh staging tree, never an installation directory.
    with tempfile.TemporaryDirectory(prefix=".agentdesk-package-", dir=output) as temporary:
        scratch = Path(temporary)
        staging = scratch / name
        staging.mkdir()
        binary_name = "agentdesk.exe" if system == "windows" else "agentdesk"
        copy_file(binary, staging / binary_name, binary.parent)
        (staging / binary_name).chmod(0o755)
        assets = git(root, "ls-files", "-z", "--", *RUNTIME_ASSETS).split("\0")
        for relative in filter(None, assets):
            copy_file(root / relative, staging / relative, root)
        if dashboard:
            dashboard_root = root / "dashboard/dist"
            for source in sorted(dashboard_root.rglob("*")):
                if source.is_symlink():
                    raise ValueError(f"dashboard asset is a symlink: {source}")
                if source.is_file():
                    copy_file(source, staging / "dashboard/dist" / source.relative_to(dashboard_root), dashboard_root)
        (staging / "VERSION").write_text(f"{version}\n", encoding="utf-8")
        source_manifest = {
            "generated_at": generated, "repo_head": head, "repo_dirty": str(dirty).lower(),
            "latest_postgres_migration": migrations[-1].name if migrations else None,
            "build_profile": profile,
        }
        write_json(staging / "runtime/release-source.json", source_manifest)
        manifest = {
            "schema_version": 1, "version": version, "target": target,
            "dashboard_included": dashboard, **source_manifest,
            "files": {p.relative_to(staging).as_posix(): sha256(p) for p in sorted(staging.rglob("*")) if p.is_file()},
        }
        write_json(staging / "release-manifest.json", manifest)
        archive = scratch / filename
        if system == "windows":
            with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as bundle:
                for path in sorted(staging.rglob("*")):
                    if path.is_file():
                        bundle.write(path, path.relative_to(scratch).as_posix())
        else:
            with tarfile.open(archive, "w:gz", compresslevel=9) as bundle:
                bundle.add(staging, arcname=name)
        digest = sha256(archive)
        os.replace(archive, output / filename)
        (output / f"{filename}.sha256").write_text(f"{digest}  {filename}\n", encoding="utf-8", newline="\n")
        # Preserve install.sh's checksums.txt contract for sequential builds.
        # The Actions publish job aggregates these per-artifact sidecars.
        checksums = "".join(p.read_text(encoding="utf-8") for p in sorted(output.glob("agentdesk-*.sha256")))
        (output / "checksums.txt").write_text(checksums, encoding="utf-8", newline="\n")
    return output / filename


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--target", required=True, choices=TARGETS)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--without-dashboard", action="store_true")
    parser.add_argument("--profile", choices=["release", "release-fast"], default="release")
    args = parser.parse_args()
    try:
        artifact = package(args.root.resolve(), args.binary.resolve(), args.target,
                           (args.output or args.root / "dist").resolve(), not args.without_dashboard, args.profile)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Release packaging failed: {error}\n")
    print(f"Artifact: {artifact}\nSHA-256: {sha256(artifact)}")


if __name__ == "__main__":
    main()
