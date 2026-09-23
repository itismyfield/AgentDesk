#!/usr/bin/env python3
"""Verify the exact release matrix and its contents before publishing."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import tarfile
import zipfile

from package_release import TARGETS, sha256


def verify_archive(archive: Path, commit: str, version: str, *, allow_dirty: bool = False, profile: str | None = None) -> str:
    sidecar = archive.with_name(archive.name + ".sha256")
    checksum = sidecar.read_text(encoding="utf-8").split()
    if checksum != [sha256(archive), archive.name]:
        raise ValueError(f"archive checksum mismatch: {archive.name}")
    prefix = archive.name.removesuffix(".tar.gz").removesuffix(".zip")
    with zipfile.ZipFile(archive) if archive.suffix == ".zip" else tarfile.open(archive) as bundle:
        if isinstance(bundle, zipfile.ZipFile):
            members = bundle.infolist()
            names = [m.filename for m in members if not m.is_dir()]
            if any((m.external_attr >> 16) & 0o170000 == 0o120000 for m in members):
                raise ValueError("release contains a symlink")
            open_member = bundle.open
        else:
            members = bundle.getmembers()
            if any(not (m.isfile() or m.isdir()) for m in members):
                raise ValueError("release contains a link or special file")
            names = [m.name for m in members if m.isfile()]
            open_member = bundle.extractfile
        if len(names) != len(set(names)):
            raise ValueError("release contains duplicate paths")
        for name in names:
            path = PurePosixPath(name)
            if path.is_absolute() or ".." in path.parts or "\\" in name or path.parts[0] != prefix:
                raise ValueError(f"unsafe release member: {name}")
        manifest_path = f"{prefix}/release-manifest.json"
        with open_member(manifest_path) as stream:
            raw = stream.read(4 * 1024 * 1024 + 1)
        if len(raw) > 4 * 1024 * 1024:
            raise ValueError("release manifest exceeds 4 MiB")
        manifest = json.loads(raw)
        target = manifest.get("target")
        if target not in TARGETS or manifest.get("schema_version") != 1:
            raise ValueError("unsupported release target or manifest schema")
        system, arch = TARGETS[target]
        expected_name = f"agentdesk-{system}-{arch}.{'zip' if system == 'windows' else 'tar.gz'}"
        if archive.name != expected_name:
            raise ValueError("archive name disagrees with its target")
        if manifest.get("repo_head") != commit or manifest.get("version") != version:
            raise ValueError("release source/version does not match the publishing commit")
        if manifest.get("build_profile") not in {"release", "release-fast"} or (profile and manifest["build_profile"] != profile):
            raise ValueError("release build profile does not match the publishing profile")
        if not allow_dirty and manifest.get("repo_dirty") != "false":
            raise ValueError("refusing to publish an artifact from a dirty checkout")
        if manifest.get("dashboard_included") is not True:
            raise ValueError("the common published release must include the dashboard")
        expected_files = manifest.get("files", {})
        actual_files = {name.removeprefix(prefix + "/") for name in names if name != manifest_path}
        if set(expected_files) != actual_files:
            raise ValueError("archive contents differ from the release manifest")
        required = {"agentdesk.exe" if system == "windows" else "agentdesk",
                    "VERSION", "runtime/release-source.json", "dashboard/dist/index.html"}
        if not required.issubset(actual_files):
            raise ValueError("required runtime files are missing")
        for relative, expected_digest in expected_files.items():
            digest = hashlib.sha256()
            with open_member(f"{prefix}/{relative}") as stream:
                for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                    digest.update(chunk)
            if digest.hexdigest() != expected_digest:
                raise ValueError(f"runtime file checksum mismatch: {relative}")
        with open_member(f"{prefix}/runtime/release-source.json") as stream:
            runtime = json.load(stream)
        if any(runtime.get(k) != manifest.get(k) for k in (
                "repo_head", "repo_dirty", "latest_postgres_migration", "generated_at", "build_profile")):
            raise ValueError("runtime source identity differs from the release manifest")
    return target


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--target", action="append", required=True, choices=TARGETS)
    parser.add_argument("--profile", choices=["release", "release-fast"])
    args = parser.parse_args()
    archives = sorted([*args.directory.glob("agentdesk-*.tar.gz"), *args.directory.glob("agentdesk-*.zip")])
    try:
        targets = [verify_archive(p, args.commit, args.version, profile=args.profile) for p in archives]
        if sorted(targets) != sorted(args.target):
            raise ValueError(f"release matrix incomplete or duplicated: expected {args.target}, got {targets}")
        checksums = "".join(f"{sha256(p)}  {p.name}\n" for p in archives)
        (args.directory / "checksums.txt").write_text(checksums, encoding="utf-8", newline="\n")
    except (OSError, ValueError, KeyError, tarfile.TarError, zipfile.BadZipFile) as error:
        parser.exit(1, f"Release verification failed: {error}\n")
    print(f"Verified {len(archives)} release artifacts for {args.commit}")


if __name__ == "__main__":
    main()
