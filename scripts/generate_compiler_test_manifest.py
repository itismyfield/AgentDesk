#!/usr/bin/env python3
"""Generate or verify the tracked compiler-owned Rust test baseline.

The manifest is data authority for later test-lane enforcement. This bootstrap
only records what Cargo and libtest emit; it does not compare a candidate with a
base and does not decide whether a lane covers a changed test.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterable, Mapping, Sequence

REPO_ROOT = Path(__file__).resolve().parent.parent
MANIFEST_REL = Path("scripts/compiler_test_manifest.json")
SCHEMA_VERSION = 1
COMPILE_COMMAND = (
    "cargo",
    "test",
    "--workspace",
    "--all-targets",
    "--no-run",
    "--message-format=json-render-diagnostics",
)
LIST_COMMAND = ("--list", "--format", "terse")


@dataclass(frozen=True)
class TestExecutable:
    package: str
    target_kind: str
    target_name: str
    executable: Path

    @property
    def target_id(self) -> str:
        return f"{self.package}::{self.target_kind}::{self.target_name}"


def canonical_json(value: object) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def content_sha256(records: Sequence[object]) -> str:
    return "sha256:" + hashlib.sha256(canonical_json(records).encode()).hexdigest()


def package_labels(metadata: Mapping[str, object], repo_root: Path) -> dict[str, str]:
    labels: dict[str, str] = {}
    for package in metadata.get("packages", []):
        manifest = Path(str(package["manifest_path"])).resolve()
        try:
            relative = manifest.relative_to(repo_root.resolve()).as_posix()
        except ValueError as exc:
            raise ValueError(f"workspace package is outside repository: {manifest}") from exc
        package_id = str(package["id"])
        labels[package_id] = f"{relative}#{package['name']}"
    return labels


def doctest_exclusions(
    metadata: Mapping[str, object], labels: Mapping[str, str]
) -> list[dict[str, str]]:
    excluded: list[dict[str, str]] = []
    for package in metadata.get("packages", []):
        package_label = labels[str(package["id"])]
        for target in package.get("targets", []):
            if target.get("doctest"):
                excluded.append(
                    {
                        "package": package_label,
                        "target_kind": "+".join(sorted(target["kind"])),
                        "target_name": str(target["name"]),
                        "reason": "rustdoc test identities are not stable source-independent IDs",
                    }
                )
    return sorted(
        excluded,
        key=lambda item: (item["package"], item["target_kind"], item["target_name"]),
    )


def parse_test_executables(
    messages: Iterable[str], labels: Mapping[str, str]
) -> list[TestExecutable]:
    executables: dict[tuple[str, str, str], TestExecutable] = {}
    for raw in messages:
        raw = raw.strip()
        if not raw:
            continue
        try:
            message = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if message.get("reason") != "compiler-artifact" or not message.get("executable"):
            continue
        profile = message.get("profile", {})
        if not profile.get("test"):
            continue
        package_id = str(message["package_id"])
        if package_id not in labels:
            continue
        target = message["target"]
        kinds = tuple(sorted(str(kind) for kind in target.get("kind", [])))
        supported = [kind for kind in kinds if kind in {"lib", "bin", "test"}]
        if len(supported) != 1:
            continue
        target_kind = supported[0]
        record = TestExecutable(
            labels[package_id],
            target_kind,
            str(target["name"]),
            Path(str(message["executable"])),
        )
        key = (record.package, record.target_kind, record.target_name)
        previous = executables.get(key)
        if previous is not None and previous.executable != record.executable:
            raise ValueError(f"multiple test executables for target {record.target_id}")
        executables[key] = record
    return [executables[key] for key in sorted(executables)]


def parse_libtest_listing(output: str) -> tuple[str, ...]:
    names: list[str] = []
    for line in output.splitlines():
        if line.endswith(": test"):
            name = line.removesuffix(": test")
            if not name or "\n" in name or "\r" in name:
                raise ValueError(f"invalid libtest identity: {name!r}")
            names.append(name)
    if len(names) != len(set(names)):
        raise ValueError("libtest emitted duplicate test identities")
    return tuple(sorted(names))


def load_source_inventory(repo_root: Path) -> dict[str, set[str]]:
    scanner_path = repo_root / "scripts/check_test_lane_coverage.py"
    spec = importlib.util.spec_from_file_location("_test_lane_scanner", scanner_path)
    if spec is None or spec.loader is None:
        raise ValueError(f"cannot load source inventory scanner: {scanner_path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module.discover_test_inventory(repo_root)


def source_owners(inventory: Mapping[str, set[str]]) -> dict[str, tuple[str, str]]:
    owners: dict[str, tuple[str, str]] = {}
    for module, tests in inventory.items():
        for test in tests:
            previous = owners.get(test)
            if previous is not None and previous != (module, test):
                raise ValueError(f"source scanner emitted duplicate owner for {test}")
            owners[test] = (module, test)
    return owners


def resolve_owner(
    compiler_name: str, owners: Mapping[str, tuple[str, str]]
) -> dict[str, object]:
    exact = owners.get(compiler_name)
    if exact is not None:
        return {
            "kind": "source",
            "resolution": "exact_source_name",
            "module": exact[0],
            "source_test_id": exact[1],
        }

    compiler_parts = compiler_name.split("::")
    ranked: list[tuple[int, tuple[str, str]]] = []
    for source_name, owner in owners.items():
        common = 0
        for left, right in zip(
            reversed(compiler_parts), reversed(source_name.split("::"))
        ):
            if left != right:
                break
            common += 1
        if common >= 3:
            ranked.append((common, owner))
    if ranked:
        best = max(common for common, _ in ranked)
        matches = sorted({owner for common, owner in ranked if common == best})
        if len(matches) == 1:
            return {
                "kind": "source",
                "resolution": "unique_structural_suffix",
                "module": matches[0][0],
                "source_test_id": matches[0][1],
            }
    return {
        "kind": "generated_or_unresolved",
        "resolution": "no_unambiguous_source_item",
        "module": None,
        "source_test_id": None,
    }


def build_manifest(
    metadata: Mapping[str, object],
    compiler_messages: Iterable[str],
    listings: Mapping[str, str],
    source_inventory: Mapping[str, set[str]],
    repo_root: Path,
    *,
    environment: Mapping[str, str] | None = None,
) -> dict[str, object]:
    labels = package_labels(metadata, repo_root)
    executables = parse_test_executables(compiler_messages, labels)
    if not executables:
        raise ValueError("Cargo emitted no test executables")
    owners = source_owners(source_inventory)
    records: list[dict[str, object]] = []
    target_counts: list[dict[str, object]] = []
    unknown = 0
    for executable in executables:
        if executable.target_id not in listings:
            raise ValueError(f"missing libtest listing for {executable.target_id}")
        names = parse_libtest_listing(listings[executable.target_id])
        target_counts.append(
            {
                "target_id": executable.target_id,
                "test_count": len(names),
                "non_vacuous": bool(names),
            }
        )
        for name in names:
            owner = resolve_owner(name, owners)
            if owner["kind"] != "source":
                unknown += 1
            records.append(
                {
                    "id": f"{executable.target_id}::{name}",
                    "package": executable.package,
                    "target_kind": executable.target_kind,
                    "target_name": executable.target_name,
                    "test_name": name,
                    "owner": owner,
                }
            )
    records.sort(key=lambda item: str(item["id"]))
    target_counts.sort(key=lambda item: str(item["target_id"]))
    environment = environment or {}
    return {
        "schema_version": SCHEMA_VERSION,
        "authority": "cargo_compiler_artifacts_and_libtest_list",
        "environment": {
            "host": environment.get("host", "unknown"),
            "rustc_release": environment.get("rustc_release", "unknown"),
            "rustc_host": environment.get("rustc_host", "unknown"),
        },
        "compile_command": list(COMPILE_COMMAND),
        "list_command": list(LIST_COMMAND),
        "normalization": "package-manifest#name::target-kind::target-name::libtest-name",
        "doctests": {
            "included": False,
            "exclusions": doctest_exclusions(metadata, labels),
        },
        "summary": {
            "target_count": len(target_counts),
            "test_count": len(records),
            "source_owned_count": len(records) - unknown,
            "generated_or_unresolved_count": unknown,
            "records_sha256": content_sha256(records),
        },
        "targets": target_counts,
        "tests": records,
    }


def render_manifest(manifest: Mapping[str, object]) -> str:
    return json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True) + "\n"


def run_checked(
    command: Sequence[str],
    cwd: Path,
    *,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
) -> subprocess.CompletedProcess:
    result = runner(
        list(command),
        cwd=cwd,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise ValueError(f"command failed ({' '.join(command)}): {detail}")
    return result


def rustc_environment(repo_root: Path) -> dict[str, str]:
    result = run_checked(("rustc", "-vV"), repo_root)
    values = dict(
        line.split(": ", 1)
        for line in result.stdout.splitlines()
        if ": " in line
    )
    return {
        "host": sys.platform,
        "rustc_release": values.get("release", "unknown"),
        "rustc_host": values.get("host", "unknown"),
    }


def generate(repo_root: Path) -> dict[str, object]:
    metadata_result = run_checked(
        ("cargo", "metadata", "--no-deps", "--format-version", "1"), repo_root
    )
    metadata = json.loads(metadata_result.stdout)
    labels = package_labels(metadata, repo_root)
    compile_result = run_checked(COMPILE_COMMAND, repo_root)
    executables = parse_test_executables(compile_result.stdout.splitlines(), labels)
    listings = {
        executable.target_id: run_checked(
            (str(executable.executable), *LIST_COMMAND), repo_root
        ).stdout
        for executable in executables
    }
    return build_manifest(
        metadata,
        compile_result.stdout.splitlines(),
        listings,
        load_source_inventory(repo_root),
        repo_root,
        environment=rustc_environment(repo_root),
    )


def check_bytes(expected: str, manifest_path: Path) -> None:
    if not manifest_path.is_file():
        raise ValueError(f"tracked compiler test manifest is missing: {manifest_path}")
    actual = manifest_path.read_text(encoding="utf-8")
    if actual != expected:
        raise ValueError(
            "tracked compiler test manifest does not match compiler output; "
            "regenerate it from the reviewed main snapshot"
        )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--check", action="store_true")
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--manifest", type=Path, default=None)
    args = parser.parse_args(argv)
    repo_root = args.repo_root.resolve()
    manifest_path = (
        args.manifest.resolve() if args.manifest else repo_root / MANIFEST_REL
    )
    try:
        rendered = render_manifest(generate(repo_root))
        if args.check:
            check_bytes(rendered, manifest_path)
            print(f"OK: {manifest_path} matches compiler test listing")
        else:
            manifest_path.write_text(rendered, encoding="utf-8")
            print(f"wrote {manifest_path}")
        return 0
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
