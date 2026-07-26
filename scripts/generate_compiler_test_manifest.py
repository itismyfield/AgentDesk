#!/usr/bin/env python3
"""Generate or verify the tracked compiler-owned Rust test baseline.

The manifest is data authority for later test-lane enforcement. This bootstrap
records what Cargo and libtest emit, but does not compare a candidate with a base
or decide whether a lane covers a changed test.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterable, Mapping, Sequence

REPO_ROOT = Path(__file__).resolve().parent.parent
MANIFEST_REL = Path("scripts/compiler_test_manifest.json")
SCHEMA_VERSION = 1
PINNED_RUSTC_RELEASE = "1.94.1"
COMPILE_COMMAND = (
    "cargo",
    "test",
    "--workspace",
    "--all-targets",
    "--no-run",
    "--message-format=json-render-diagnostics",
)
LIST_COMMAND = ("--list", "--format", "terse")
SUPPORTED_TARGET_KINDS = frozenset({"lib", "bin", "test"})
ZERO_TEST_ALLOWLIST = {
    "Cargo.toml#agentdesk::bin::agentdesk": (
        "binary entry point has no unit-test module; tests live in the library harness"
    ),
}
LIBTEST_SUMMARY_RE = re.compile(r"^(?P<tests>\d+) tests?, (?P<benches>\d+) benchmarks?$")


@dataclass(frozen=True)
class TestTarget:
    package: str
    package_id: str
    target_kind: str
    target_name: str

    @property
    def target_id(self) -> str:
        return f"{self.package}::{self.target_kind}::{self.target_name}"


@dataclass(frozen=True)
class TestExecutable:
    target: TestTarget
    executable: Path

    @property
    def target_id(self) -> str:
        return self.target.target_id


def canonical_json(value: object) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def content_sha256(records: Sequence[object]) -> str:
    return "sha256:" + hashlib.sha256(canonical_json(records).encode()).hexdigest()


def package_labels(metadata: Mapping[str, object], repo_root: Path) -> dict[str, str]:
    labels: dict[str, str] = {}
    workspace_members = {str(member) for member in metadata.get("workspace_members", [])}
    for package in metadata.get("packages", []):
        package_id = str(package["id"])
        if workspace_members and package_id not in workspace_members:
            continue
        manifest = Path(str(package["manifest_path"])).resolve()
        try:
            relative = manifest.relative_to(repo_root.resolve()).as_posix()
        except ValueError as exc:
            raise ValueError(f"workspace package is outside repository: {manifest}") from exc
        labels[package_id] = f"{relative}#{package['name']}"
    if not labels:
        raise ValueError("Cargo metadata contains no workspace packages")
    return labels


def expected_test_targets(
    metadata: Mapping[str, object], labels: Mapping[str, str]
) -> dict[str, TestTarget]:
    expected: dict[str, TestTarget] = {}
    packages = {str(package["id"]): package for package in metadata.get("packages", [])}
    for package_id, package_label in labels.items():
        package = packages[package_id]
        for raw_target in package.get("targets", []):
            if not raw_target.get("test"):
                continue
            kinds = tuple(sorted(str(kind) for kind in raw_target.get("kind", [])))
            supported = [kind for kind in kinds if kind in SUPPORTED_TARGET_KINDS]
            if len(supported) != 1:
                raise ValueError(
                    f"unsupported test-capable Cargo target kinds for "
                    f"{package_label}::{raw_target.get('name')}: {kinds}"
                )
            target = TestTarget(
                package=package_label,
                package_id=package_id,
                target_kind=supported[0],
                target_name=str(raw_target["name"]),
            )
            if target.target_id in expected:
                raise ValueError(f"duplicate Cargo metadata target {target.target_id}")
            expected[target.target_id] = target
    if not expected:
        raise ValueError("Cargo metadata contains no supported test-capable targets")
    return expected


def doctest_exclusions(
    metadata: Mapping[str, object], labels: Mapping[str, str]
) -> list[dict[str, str]]:
    excluded: list[dict[str, str]] = []
    for package in metadata.get("packages", []):
        package_id = str(package["id"])
        if package_id not in labels:
            continue
        for target in package.get("targets", []):
            if target.get("doctest"):
                excluded.append(
                    {
                        "package": labels[package_id],
                        "target_kind": "+".join(sorted(target["kind"])),
                        "target_name": str(target["name"]),
                        "reason": (
                            "rustdoc test identities are not stable "
                            "source-independent IDs"
                        ),
                    }
                )
    return sorted(
        excluded,
        key=lambda item: (item["package"], item["target_kind"], item["target_name"]),
    )


def parse_test_executables(
    messages: Iterable[str],
    expected: Mapping[str, TestTarget],
    artifact_root: Path | None = None,
) -> list[TestExecutable]:
    by_cargo_key = {
        (target.package_id, target.target_kind, target.target_name): target
        for target in expected.values()
    }
    executables: dict[str, TestExecutable] = {}
    canonical_root = artifact_root.resolve() if artifact_root is not None else None
    for raw in messages:
        raw = raw.strip()
        if not raw:
            continue
        try:
            message = json.loads(raw)
        except json.JSONDecodeError as exc:
            raise ValueError(f"Cargo emitted malformed JSON output: {raw!r}") from exc
        if message.get("reason") != "compiler-artifact" or not message.get("executable"):
            continue
        if not message.get("profile", {}).get("test"):
            continue
        package_id = str(message.get("package_id", ""))
        cargo_target = message.get("target", {})
        kinds = tuple(sorted(str(kind) for kind in cargo_target.get("kind", [])))
        supported = [kind for kind in kinds if kind in SUPPORTED_TARGET_KINDS]
        if len(supported) != 1:
            raise ValueError(f"unexpected executable test artifact kinds: {kinds}")
        key = (package_id, supported[0], str(cargo_target.get("name", "")))
        target = by_cargo_key.get(key)
        if target is None:
            raise ValueError(f"unexpected non-workspace or non-test Cargo executable: {key}")
        executable_path = Path(str(message["executable"]))
        if canonical_root is not None:
            try:
                executable_path = executable_path.resolve(strict=True)
                executable_path.relative_to(canonical_root)
            except (FileNotFoundError, ValueError) as exc:
                raise ValueError(
                    f"Cargo executable is missing or outside fresh target output: "
                    f"{executable_path}"
                ) from exc
            if not executable_path.is_file():
                raise ValueError(f"Cargo executable is not a regular file: {executable_path}")
        if target.target_id in executables:
            raise ValueError(f"multiple test executables for target {target.target_id}")
        executables[target.target_id] = TestExecutable(target, executable_path)

    missing = sorted(set(expected) - set(executables))
    if missing:
        raise ValueError(f"Cargo emitted no test executable for metadata targets: {missing}")
    return [executables[target_id] for target_id in sorted(executables)]


def parse_libtest_listing(output: str) -> tuple[str, ...]:
    names: list[str] = []
    declared_count: int | None = None
    for raw_line in output.splitlines():
        line = raw_line.strip()
        if not line:
            continue
        summary = LIBTEST_SUMMARY_RE.fullmatch(line)
        if summary:
            if declared_count is not None:
                raise ValueError("libtest emitted multiple listing summaries")
            declared_count = int(summary.group("tests"))
            if int(summary.group("benches")) != 0:
                raise ValueError("libtest benchmarks require a separate identity contract")
            continue
        if not line.endswith(": test"):
            raise ValueError(f"unsupported libtest listing line: {raw_line!r}")
        name = line.removesuffix(": test")
        if not name:
            raise ValueError("libtest emitted an empty test identity")
        names.append(name)
    if declared_count is not None and declared_count != len(names):
        raise ValueError(
            f"libtest summary declared {declared_count} tests but listed {len(names)}"
        )
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
    inventory = module.discover_test_inventory(repo_root, include_external_tests=True)
    integration_root = repo_root / "tests" / "e2e"
    if integration_root.is_dir():
        for path in sorted(integration_root.rglob("*.rs")):
            if path.name == "main.rs":
                continue
            relative = path.relative_to(integration_root)
            base = (*relative.parent.parts, relative.stem)
            source = path.read_text(encoding="utf-8")
            test_paths = module._test_function_paths(source, base)
            if test_paths:
                inventory.setdefault("::".join(base), set()).update(
                    "::".join(test_path) for test_path in test_paths
                )
    return inventory


def source_owners(inventory: Mapping[str, set[str]]) -> dict[str, tuple[str, str]]:
    candidates: dict[str, set[tuple[str, str]]] = {}
    for module, tests in inventory.items():
        for test in tests:
            candidates.setdefault(test, set()).add((module, test))
    return {
        test: next(iter(matches))
        for test, matches in candidates.items()
        if len(matches) == 1
    }


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


def classify_unresolved_owner(compiler_name: str) -> dict[str, object]:
    integration_source = REPO_ROOT / "tests" / "e2e" / f"{compiler_name.split('::', 1)[0]}.rs"
    if integration_source.is_file():
        return {
            "kind": "generated_or_unresolved",
            "resolution": "integration_source_outside_library_scanner",
            "module": compiler_name.rsplit("::", 1)[0],
            "source_test_id": compiler_name,
        }
    include_markers = (
        "::flake_isolation_",
        "::provider_output_guard_tests::",
        "::task_notification_kind_restart_invariant_tests::",
    )
    if any(marker in compiler_name for marker in include_markers):
        resolution = "included_or_nested_test_source"
    elif "::terminal_direct_fallback::tests::" in compiler_name or "::pcm_harness_tests::" in compiler_name:
        resolution = "nested_external_test_source"
    else:
        resolution = "generated_or_macro_expanded_source"
    return {
        "kind": "generated_or_unresolved",
        "resolution": resolution,
        "module": compiler_name.rsplit("::", 1)[0],
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
    artifact_root: Path | None = None,
) -> dict[str, object]:
    labels = package_labels(metadata, repo_root)
    expected = expected_test_targets(metadata, labels)
    executables = parse_test_executables(compiler_messages, expected, artifact_root)
    expected_ids = set(expected)
    if set(listings) != expected_ids:
        missing = sorted(expected_ids - set(listings))
        extra = sorted(set(listings) - expected_ids)
        raise ValueError(f"libtest listing target mismatch; missing={missing}, extra={extra}")

    owners = source_owners(source_inventory)
    records: list[dict[str, object]] = []
    target_counts: list[dict[str, object]] = []
    unknown = 0
    for executable in executables:
        names = parse_libtest_listing(listings[executable.target_id])
        zero_reason = ZERO_TEST_ALLOWLIST.get(executable.target_id)
        if not names and zero_reason is None:
            raise ValueError(
                f"test-capable target unexpectedly listed zero tests: {executable.target_id}"
            )
        if names and zero_reason is not None:
            raise ValueError(
                f"zero-test allowlist is stale for nonempty target: {executable.target_id}"
            )
        target_counts.append(
            {
                "target_id": executable.target_id,
                "status": "listed",
                "test_count": len(names),
                "non_vacuous": bool(names),
                "zero_test_allowance": zero_reason,
            }
        )
        for name in names:
            owner = resolve_owner(name, owners)
            if owner["kind"] != "source":
                owner = classify_unresolved_owner(name)
                unknown += 1
            records.append(
                {
                    "id": f"{executable.target_id}::{name}",
                    "package": executable.target.package,
                    "target_kind": executable.target.target_kind,
                    "target_name": executable.target.target_name,
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
        "authority_scope": {
            "records_and_targets": "cross_host",
            "environment": "provenance_only",
            "pinned_rustc_release": PINNED_RUSTC_RELEASE,
        },
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
    env: Mapping[str, str] | None = None,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
) -> subprocess.CompletedProcess:
    result = runner(
        list(command),
        cwd=cwd,
        env=env,
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
        line.split(": ", 1) for line in result.stdout.splitlines() if ": " in line
    )
    release = values.get("release", "unknown")
    if release != PINNED_RUSTC_RELEASE:
        raise ValueError(
            f"compiler manifest requires rustc {PINNED_RUSTC_RELEASE}, found {release}"
        )
    return {
        "host": sys.platform,
        "rustc_release": release,
        "rustc_host": values.get("host", "unknown"),
    }


def generate(repo_root: Path) -> dict[str, object]:
    metadata_result = run_checked(
        ("cargo", "metadata", "--no-deps", "--format-version", "1"), repo_root
    )
    metadata = json.loads(metadata_result.stdout)
    labels = package_labels(metadata, repo_root)
    expected = expected_test_targets(metadata, labels)
    with tempfile.TemporaryDirectory(prefix="compiler-test-manifest-target-") as temp_target:
        artifact_root = Path(temp_target).resolve()
        environment = dict(os.environ)
        environment["CARGO_TARGET_DIR"] = str(artifact_root)
        compile_result = run_checked(COMPILE_COMMAND, repo_root, env=environment)
        compiler_messages = compile_result.stdout.splitlines()
        executables = parse_test_executables(
            compiler_messages, expected, artifact_root
        )
        listings = {
            executable.target_id: run_checked(
                (str(executable.executable), *LIST_COMMAND), repo_root
            ).stdout
            for executable in executables
        }
        return build_manifest(
            metadata,
            compiler_messages,
            listings,
            load_source_inventory(repo_root),
            repo_root,
            environment=rustc_environment(repo_root),
            artifact_root=artifact_root,
        )


def validate_manifest(manifest: Mapping[str, object]) -> None:
    if manifest.get("schema_version") != SCHEMA_VERSION:
        raise ValueError("tracked compiler manifest has an unsupported schema version")
    tests = manifest.get("tests")
    targets = manifest.get("targets")
    summary = manifest.get("summary")
    if not isinstance(tests, list) or not isinstance(targets, list):
        raise ValueError("tracked compiler manifest records are malformed")
    if not isinstance(summary, dict):
        raise ValueError("tracked compiler manifest summary is malformed")
    if summary.get("records_sha256") != content_sha256(tests):
        raise ValueError("tracked compiler manifest record hash is stale")
    if summary.get("test_count") != len(tests):
        raise ValueError("tracked compiler manifest test count is stale")
    if summary.get("target_count") != len(targets):
        raise ValueError("tracked compiler manifest target count is stale")


def authority_projection(manifest: Mapping[str, object]) -> dict[str, object]:
    validate_manifest(manifest)
    return {key: value for key, value in manifest.items() if key != "environment"}


def check_manifest(expected: Mapping[str, object], manifest_path: Path) -> None:
    if not manifest_path.is_file():
        raise ValueError(f"tracked compiler test manifest is missing: {manifest_path}")
    actual_text = manifest_path.read_text(encoding="utf-8")
    tracked = json.loads(actual_text)
    if actual_text != render_manifest(tracked):
        raise ValueError("tracked compiler test manifest is not canonical JSON")
    if authority_projection(tracked) != authority_projection(expected):
        expected_summary = expected.get("summary", {})
        tracked_summary = tracked.get("summary", {})
        raise ValueError(
            "tracked compiler test manifest does not match fresh compiler output; "
            f"tracked={tracked_summary}, fresh={expected_summary}"
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
        generated = generate(repo_root)
        if args.check:
            check_manifest(generated, manifest_path)
            print(f"OK: {manifest_path} matches fresh compiler test listing")
        else:
            manifest_path.write_text(render_manifest(generated), encoding="utf-8")
            print(f"wrote {manifest_path}")
        return 0
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
