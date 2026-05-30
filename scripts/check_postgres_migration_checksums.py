#!/usr/bin/env python3
"""Guard PostgreSQL migration files against checksum drift.

Numbered migrations are embedded into the Rust binary by sqlx at compile time
and are also recorded in live databases in `_sqlx_migrations`. Editing an
already-applied migration changes the resolved checksum and can block deploys.

The immutable manifest records the checksum baseline for each numbered
PostgreSQL migration. New migration files must be added to the manifest. Edits
to an existing migration must keep the baseline unchanged and add an explicit
entry to the repair allowlist with the old and new checksum.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any


DEFAULT_MANIFEST = Path("migrations/postgres/immutable-checksums.json")
DEFAULT_ALLOWLIST = Path("migrations/postgres/checksum-repair-allowlist.json")
MIGRATION_NAME_RE = re.compile(r"^(?P<version>[0-9]{4})_.+\.sql$")
HEX64_RE = re.compile(r"^[0-9a-f]{64}$")


@dataclass(frozen=True)
class Migration:
    path: str
    version: int
    sha256: str


@dataclass(frozen=True)
class RepairEntry:
    path: str
    old_sha256: str
    new_sha256: str
    issue: str
    reason: str
    covered_by_migration: str
    repair_doc: str

    @property
    def key(self) -> tuple[str, str, str]:
        return (self.path, self.old_sha256, self.new_sha256)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def migration_version_from_path(path: str) -> int | None:
    match = MIGRATION_NAME_RE.match(Path(path).name)
    if not match:
        return None
    return int(match.group("version"))


def validate_migration_relpath(path: str) -> str | None:
    rel = Path(path)
    if rel.is_absolute() or ".." in rel.parts:
        return f"path must be repository-relative and stay inside the repo: {path}"
    if rel.parts[:2] != ("migrations", "postgres"):
        return f"path must live under migrations/postgres: {path}"
    if migration_version_from_path(path) is None:
        return f"path is not a numbered postgres migration: {path}"
    return None


def find_migrations(root: Path) -> dict[str, Migration]:
    migrations_dir = root / "migrations" / "postgres"
    migrations: dict[str, Migration] = {}
    if not migrations_dir.is_dir():
        raise ValueError(f"missing migrations directory: {migrations_dir}")
    for path in sorted(migrations_dir.iterdir()):
        if not path.is_file():
            continue
        match = MIGRATION_NAME_RE.match(path.name)
        if not match:
            continue
        rel_path = path.relative_to(root).as_posix()
        migrations[rel_path] = Migration(
            path=rel_path,
            version=int(match.group("version")),
            sha256=sha256_file(path),
        )
    return migrations


def manifest_payload(migrations: dict[str, Migration]) -> dict[str, Any]:
    return {
        "version": 1,
        "policy": (
            "Numbered postgres migrations are immutable after merge. "
            "Add new migration files and append their checksum here. "
            "For intentional edits to an existing migration, keep this baseline "
            "unchanged and add migrations/postgres/checksum-repair-allowlist.json."
        ),
        "protected_migrations": [
            {
                "path": migration.path,
                "version": migration.version,
                "sha256": migration.sha256,
            }
            for migration in sorted(migrations.values(), key=lambda item: item.path)
        ],
    }


def load_json_file(path: Path) -> Any:
    with path.open("r", encoding="utf-8") as handle:
        return json.load(handle)


def parse_manifest(data: Any, source: str) -> tuple[dict[str, Migration], list[str]]:
    errors: list[str] = []
    if not isinstance(data, dict):
        return {}, [f"{source}: manifest must be a JSON object"]
    entries = data.get("protected_migrations")
    if not isinstance(entries, list):
        return {}, [f"{source}: protected_migrations must be a list"]

    manifest: dict[str, Migration] = {}
    for idx, entry in enumerate(entries):
        prefix = f"{source}: protected_migrations[{idx}]"
        if not isinstance(entry, dict):
            errors.append(f"{prefix}: entry must be an object")
            continue
        path = entry.get("path")
        sha256 = entry.get("sha256")
        version = entry.get("version")
        if not isinstance(path, str) or not path:
            errors.append(f"{prefix}: path must be a non-empty string")
            continue
        path_error = validate_migration_relpath(path)
        if path_error:
            errors.append(f"{prefix}: {path_error}")
            continue
        derived_version = migration_version_from_path(path)
        if version != derived_version:
            errors.append(
                f"{prefix}: version must match filename prefix "
                f"({derived_version}), got {version!r}"
            )
        if not isinstance(sha256, str) or not HEX64_RE.match(sha256):
            errors.append(f"{prefix}: sha256 must be a lowercase SHA-256 hex digest")
            continue
        if path in manifest:
            errors.append(f"{prefix}: duplicate manifest path {path}")
            continue
        manifest[path] = Migration(path=path, version=derived_version or -1, sha256=sha256)
    return manifest, errors


def parse_allowlist(
    data: Any,
    source: str,
) -> tuple[dict[tuple[str, str, str], RepairEntry], list[str]]:
    errors: list[str] = []
    if not isinstance(data, dict):
        return {}, [f"{source}: allowlist must be a JSON object"]
    entries = data.get("repairs", [])
    if not isinstance(entries, list):
        return {}, [f"{source}: repairs must be a list"]

    repairs: dict[tuple[str, str, str], RepairEntry] = {}
    required = [
        "path",
        "old_sha256",
        "new_sha256",
        "issue",
        "reason",
        "covered_by_migration",
        "repair_doc",
    ]
    for idx, entry in enumerate(entries):
        prefix = f"{source}: repairs[{idx}]"
        if not isinstance(entry, dict):
            errors.append(f"{prefix}: entry must be an object")
            continue
        missing = [
            field
            for field in required
            if not isinstance(entry.get(field), str) or not entry.get(field).strip()
        ]
        if missing:
            errors.append(f"{prefix}: missing non-empty field(s): {', '.join(missing)}")
            continue
        path = entry["path"]
        path_error = validate_migration_relpath(path)
        if path_error:
            errors.append(f"{prefix}: {path_error}")
            continue
        old_sha = entry["old_sha256"]
        new_sha = entry["new_sha256"]
        if not HEX64_RE.match(old_sha):
            errors.append(f"{prefix}: old_sha256 must be a lowercase SHA-256 hex digest")
            continue
        if not HEX64_RE.match(new_sha):
            errors.append(f"{prefix}: new_sha256 must be a lowercase SHA-256 hex digest")
            continue
        repair = RepairEntry(
            path=path,
            old_sha256=old_sha,
            new_sha256=new_sha,
            issue=entry["issue"].strip(),
            reason=entry["reason"].strip(),
            covered_by_migration=entry["covered_by_migration"].strip(),
            repair_doc=entry["repair_doc"].strip(),
        )
        if repair.key in repairs:
            errors.append(f"{prefix}: duplicate repair entry for {path}")
            continue
        repairs[repair.key] = repair
    return repairs, errors


def load_manifest(path: Path, source: str) -> tuple[dict[str, Migration], list[str]]:
    try:
        return parse_manifest(load_json_file(path), source)
    except FileNotFoundError:
        return {}, [f"{source}: missing manifest {path}"]
    except json.JSONDecodeError as exc:
        return {}, [f"{source}: invalid JSON: {exc}"]


def load_allowlist(
    path: Path,
    source: str,
) -> tuple[dict[tuple[str, str, str], RepairEntry], list[str]]:
    if not path.exists():
        return {}, []
    try:
        return parse_allowlist(load_json_file(path), source)
    except json.JSONDecodeError as exc:
        return {}, [f"{source}: invalid JSON: {exc}"]


def load_base_manifest_from_git(
    root: Path,
    base_ref: str,
    manifest_relpath: str,
) -> tuple[dict[str, Migration] | None, list[str], str | None]:
    if not base_ref:
        return None, [], None
    show_ref = f"{base_ref}:{manifest_relpath}"
    try:
        result = subprocess.run(
            ["git", "-C", str(root), "show", show_ref],
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError as exc:
        return None, [f"git unavailable while reading {show_ref}: {exc}"], None
    if result.returncode != 0:
        return None, [], f"{show_ref} unavailable"
    try:
        data = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        return None, [f"{show_ref}: invalid JSON: {exc}"], None
    manifest, errors = parse_manifest(data, show_ref)
    return manifest, errors, None


def repair_for(
    repairs: dict[tuple[str, str, str], RepairEntry],
    path: str,
    old_sha256: str,
    new_sha256: str,
) -> RepairEntry | None:
    return repairs.get((path, old_sha256, new_sha256))


def check_migrations(
    root: Path,
    manifest_path: Path,
    allowlist_path: Path,
    base_ref: str,
    base_manifest_path: Path | None,
) -> tuple[list[str], list[str], list[str]]:
    errors: list[str] = []
    warnings: list[str] = []
    allowed: list[str] = []
    used_repairs: set[tuple[str, str, str]] = set()

    migrations = find_migrations(root)
    manifest, manifest_errors = load_manifest(manifest_path, str(manifest_path))
    repairs, repair_errors = load_allowlist(allowlist_path, str(allowlist_path))
    errors.extend(manifest_errors)
    errors.extend(repair_errors)
    if errors:
        return errors, warnings, allowed

    base_manifest: dict[str, Migration] | None = None
    if base_manifest_path is not None:
        base_manifest, base_errors = load_manifest(base_manifest_path, str(base_manifest_path))
        errors.extend(base_errors)
    else:
        base_manifest, base_errors, base_warning = load_base_manifest_from_git(
            root,
            base_ref,
            manifest_path.relative_to(root).as_posix(),
        )
        errors.extend(base_errors)
        if base_warning:
            warnings.append(f"{base_warning}; enforcing current checksum manifest only")
    if errors:
        return errors, warnings, allowed

    for path, migration in migrations.items():
        expected = manifest.get(path)
        if expected is None:
            errors.append(
                f"{path}: missing from immutable checksum manifest; "
                "append it when adding a new migration"
            )
            continue
        if expected.sha256 != migration.sha256:
            repair = repair_for(repairs, path, expected.sha256, migration.sha256)
            if repair is None:
                errors.append(
                    f"{path}: checksum drift without repair allowlist "
                    f"(manifest={expected.sha256}, current={migration.sha256})"
                )
            else:
                used_repairs.add(repair.key)
                allowed.append(
                    f"{path}: repair allowlisted by {repair.issue} "
                    f"(old={repair.old_sha256}, new={repair.new_sha256})"
                )

    for path in sorted(set(manifest) - set(migrations)):
        errors.append(f"{path}: listed in manifest but file is missing")

    if base_manifest is not None:
        for path, base in sorted(base_manifest.items()):
            current = migrations.get(path)
            current_manifest = manifest.get(path)
            if current is None:
                errors.append(f"{path}: protected migration from base manifest was deleted")
                continue
            if current_manifest is not None and current_manifest.sha256 != base.sha256:
                errors.append(
                    f"{path}: immutable manifest entry changed relative to base; keep "
                    f"sha256={base.sha256} and use the repair allowlist for intentional drift "
                    f"(manifest={current_manifest.sha256}, current={current.sha256})"
                )
                continue
            if (
                current.sha256 == base.sha256
                and current_manifest
                and current_manifest.sha256 == base.sha256
            ):
                continue
            repair = repair_for(repairs, path, base.sha256, current.sha256)
            if repair is None:
                errors.append(
                    f"{path}: existing migration changed relative to base without repair "
                    f"allowlist (base={base.sha256}, current={current.sha256})"
                )
                continue
            used_repairs.add(repair.key)
            allowed.append(
                f"{path}: base drift repair allowlisted by {repair.issue} "
                f"(old={repair.old_sha256}, new={repair.new_sha256})"
            )

    stale_repairs = sorted(set(repairs) - used_repairs)
    for path, old_sha, new_sha in stale_repairs:
        errors.append(
            f"{path}: stale repair allowlist entry is not used by current drift "
            f"(old={old_sha}, new={new_sha})"
        )

    return errors, warnings, allowed


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path.cwd(), help="repository root")
    parser.add_argument(
        "--manifest",
        type=Path,
        default=None,
        help=f"checksum manifest path (default: {DEFAULT_MANIFEST})",
    )
    parser.add_argument(
        "--allowlist",
        type=Path,
        default=None,
        help=f"repair allowlist path (default: {DEFAULT_ALLOWLIST})",
    )
    parser.add_argument(
        "--base-ref",
        default=None,
        help="git ref used to detect baseline edits; empty string disables git baseline check",
    )
    parser.add_argument(
        "--base-manifest",
        type=Path,
        default=None,
        help="load a base manifest from this file instead of git (test helper)",
    )
    parser.add_argument(
        "--print-manifest",
        action="store_true",
        help="print a manifest for the current migration tree and exit",
    )
    return parser.parse_args(argv)


def resolve_repo_path(root: Path, path: Path) -> Path:
    if path.is_absolute():
        return path.resolve()
    return (root / path).resolve()


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    root = args.root.resolve()
    manifest_path = resolve_repo_path(root, args.manifest or DEFAULT_MANIFEST)
    allowlist_path = resolve_repo_path(root, args.allowlist or DEFAULT_ALLOWLIST)
    if args.base_ref is None:
        base_ref = os.environ.get("AGENTDESK_MIGRATION_GUARD_BASE_REF") or "origin/main"
    else:
        base_ref = args.base_ref

    try:
        migrations = find_migrations(root)
    except ValueError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1

    if args.print_manifest:
        json.dump(manifest_payload(migrations), sys.stdout, indent=2, sort_keys=True)
        sys.stdout.write("\n")
        return 0

    try:
        errors, warnings, allowed = check_migrations(
            root=root,
            manifest_path=manifest_path,
            allowlist_path=allowlist_path,
            base_ref=base_ref,
            base_manifest_path=args.base_manifest,
        )
    except ValueError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1

    for warning in warnings:
        print(f"WARNING: {warning}", file=sys.stderr)
    for item in allowed:
        print(f"ALLOWLISTED: {item}")
    if errors:
        print("Postgres migration checksum guard failed:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1

    print(f"Postgres migration checksum guard passed ({len(migrations)} migrations protected)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
