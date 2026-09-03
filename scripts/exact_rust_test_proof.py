#!/usr/bin/env python3
"""Ownership and endpoint-checked source identity for exact Rust library tests."""
from __future__ import annotations

import argparse
import hashlib
import os
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

from check_test_target_integrity import (
    _rust_tokens,
    collect_static_tests,
    load_lib_inventory_manifest,
)


POLICIES = frozenset({"required", "optional"})
SEGMENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
RESULT = re.compile(
    r"^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; "
    r"[0-9]+ filtered out; finished in [0-9]+(?:\.[0-9]+)?s$"
)
ABSENT_PREFIX = "EXACT_RUST_TEST_PROOF ABSENT_CLEAN"
class ProofError(ValueError):
    """An exact-proof invariant failed."""
@dataclass(frozen=True)
class GateSpec:
    key: str
    parent_source: str
    module_ident: str
    child_source: str
    policy: str


@dataclass(frozen=True)
class OwnerSpec:
    key: str
    gate_key: str
    parent_source: str
    module_ident: str
    owner_source: str
    owner_family: str
    policy: str
    ids: tuple[str, ...]


@dataclass(frozen=True)
class ProofPlan:
    repo_root: Path
    manifest: str
    pass_prefix: str
    gate: GateSpec
    owners: tuple[OwnerSpec, ...]


@dataclass(frozen=True)
class ProofResult:
    applicable: bool
    execution_ids: tuple[str, ...]
    selected: int
    passed: int


@dataclass(frozen=True)
class AbsentCleanExpectation:
    owner_key: str

    def render(self, result: ProofResult) -> str:
        return (
            f"{ABSENT_PREFIX} owner={self.owner_key} selected={result.selected} "
            f"passed={result.passed} absent_selected=0 temp=empty"
        )


@dataclass(frozen=True)
class SealedProof:
    execution_ids: tuple[str, ...]
    absent: tuple[AbsentCleanExpectation, ...]
    identity: str
def _fail(message: str) -> None:
    raise ProofError(message)


def _segments(value: str, label: str) -> tuple[str, ...]:
    parts = tuple(value.split("::"))
    if not parts or any(not SEGMENT.fullmatch(part) for part in parts):
        _fail(f"{label} must be an exact Rust segment path: {value}")
    return parts


def _relative(value: str, label: str) -> str:
    path = PurePosixPath(value)
    if path.is_absolute() or path.as_posix() != value or not path.parts or any(part in ("", ".", "..") for part in path.parts):
        _fail(f"{label} must be one canonical repository-relative path: {value}")
    return path.as_posix()


def _path(plan: ProofPlan, value: str, label: str) -> Path:
    relative = _relative(value, label)
    root = plan.repo_root.resolve()
    joined = root.joinpath(*PurePosixPath(relative).parts)
    resolved = joined.resolve()
    if not resolved.is_relative_to(root) or (joined.exists() and resolved != joined.absolute()):
        _fail(f"{label} must not escape or alias the repository: {value}")
    return joined


def _plan_digest(plan: ProofPlan) -> str:
    return hashlib.sha256(repr(plan).encode("utf-8")).hexdigest()
def _identity(plan: ProofPlan) -> str:
    """Hash governed checkout bytes at a point in time.

    The CI runner checkout is the trusted boundary. ``run`` checks identity at
    seal and immediately before and after every credited child. On the
    applicable path, it checks once more after transcript normalization,
    reduction, and result construction but before proof records are emitted. A
    governed-source change and restore wholly within one child window is not
    detected and is outside this engine's guarantee.
    """
    paths = [plan.manifest, "src/lib.rs", plan.gate.parent_source, plan.gate.child_source]
    paths += [value for owner in plan.owners for value in (owner.parent_source, owner.owner_source)]
    digest = hashlib.sha256()
    for value in sorted(set(paths)):
        path = _path(plan, value, "identity path")
        digest.update(value.encode() + b"\0" + (path.read_bytes() if path.is_file() else b"<absent>") + b"\0")
    return digest.hexdigest()
def _module_count(source: Path, module: str) -> int:
    if not source.is_file():
        return 0
    tokens = _rust_tokens(source.read_text("utf-8"))
    count = 0
    pending_path = False
    index = 0
    while index < len(tokens):
        token = tokens[index]
        if token.value == "#" and index + 1 < len(tokens) and tokens[index + 1].value == "[":
            end, depth = index + 2, 1
            while end < len(tokens) and depth:
                depth += tokens[end].value == "["
                depth -= tokens[end].value == "]"
                end += 1
            attr = tuple(item.value for item in tokens[index + 2:end - 1] if item.kind == "ident")
            pending_path = pending_path or bool(attr and attr[0] == "path")
            index = end
            continue
        if token.value == "mod" and token.kind == "ident" and index + 2 < len(tokens):
            name, terminator = tokens[index + 1:index + 3]
            if name.kind == "ident" and name.value == module and terminator.value == ";":
                if pending_path:
                    _fail(f"module {module} must not use #[path]")
                count += 1
            pending_path = False
        elif token.value in (";", "{"):
            pending_path = False
        index += 1
    return count


def _family_ids(values: set[str] | frozenset[str], family: str) -> set[str]:
    prefix = family + "::"
    return {value for value in values if value.startswith(prefix)}


def _validate_spec(plan: ProofPlan) -> None:
    if not plan.repo_root.is_dir():
        _fail(f"repository root is not a directory: {plan.repo_root}")
    if not SEGMENT.fullmatch(plan.pass_prefix):
        _fail("pass prefix must be one Rust-style identifier")
    gate = plan.gate
    if gate.policy not in POLICIES:
        _fail(f"invalid gate policy: {gate.policy}")
    if not SEGMENT.fullmatch(gate.key) or not SEGMENT.fullmatch(gate.module_ident):
        _fail("gate key and module must be identifiers")
    _relative(gate.parent_source, "gate parent")
    _relative(gate.child_source, "gate child")
    if not plan.owners:
        _fail("at least one owner is required")
    keys: set[str] = set()
    files: set[str] = set()
    families: set[str] = set()
    full_ids: set[str] = set()
    terminals: set[str] = set()
    for owner in plan.owners:
        if owner.policy not in POLICIES or owner.gate_key != gate.key:
            _fail(f"owner {owner.key} has invalid policy or gate")
        if not SEGMENT.fullmatch(owner.key) or not SEGMENT.fullmatch(owner.module_ident):
            _fail("owner key and module must be identifiers")
        if not owner.ids:
            _fail(f"owner {owner.key} must register a nonempty immutable ID tuple")
        _relative(owner.parent_source, f"owner {owner.key} parent")
        owner_file = _relative(owner.owner_source, f"owner {owner.key} source")
        family = _segments(owner.owner_family, f"owner {owner.key} family")
        if family.count("tests") != 1 or family[-1] != "tests":
            _fail(f"owner {owner.key} family must end at one tests boundary")
        if owner.key in keys or owner_file in files or owner.owner_family in families:
            _fail(f"owner {owner.key} collides by key, source, or family")
        keys.add(owner.key)
        files.add(owner_file)
        families.add(owner.owner_family)
        local_ids: set[str] = set()
        for test_id in owner.ids:
            parts = _segments(test_id, f"owner {owner.key} ID")
            if parts.count("tests") != 1 or "::".join(parts[:-1]) != owner.owner_family:
                _fail(f"ID {test_id} does not belong exactly to {owner.owner_family}")
            terminal = parts[-1]
            if test_id in local_ids or test_id in full_ids or terminal in terminals:
                _fail(f"ID or terminal collision: {test_id}")
            local_ids.add(test_id)
            full_ids.add(test_id)
            terminals.add(terminal)


def seal(plan: ProofPlan) -> SealedProof:
    """Validate the complete graph, then commit an independent execution tuple."""
    before = _plan_digest(plan)
    _validate_spec(plan)
    manifest_path = _path(plan, plan.manifest, "manifest")
    try:
        manifest_ids = load_lib_inventory_manifest(manifest_path)
    except (OSError, ValueError) as error:
        _fail(f"manifest identity is invalid: {error}")
    crate = plan.repo_root / "src/lib.rs"
    inventory = collect_static_tests(crate, plan.repo_root)
    gate_parent = _path(plan, plan.gate.parent_source, "gate parent")
    gate_child = _path(plan, plan.gate.child_source, "gate child")
    gate_count = _module_count(gate_parent, plan.gate.module_ident)
    if gate_count not in (0, 1):
        _fail(f"gate {plan.gate.key} count={gate_count}, expected zero or one")
    if gate_count == 0:
        residue = gate_child.exists()
        for owner in plan.owners:
            residue |= _path(plan, owner.owner_source, f"owner {owner.key} source").exists()
            residue |= bool(_family_ids(set(inventory.tests), owner.owner_family))
            residue |= bool(_family_ids(manifest_ids, owner.owner_family))
        if residue or plan.gate.policy == "required":
            _fail(f"gate {plan.gate.key} is absent but required or has residue")
        absent = tuple(AbsentCleanExpectation(owner.key) for owner in plan.owners if owner.policy == "optional")
        return SealedProof((), absent, _identity(plan))
    if not gate_child.is_file():
        _fail(f"gate {plan.gate.key} child is missing")
    candidates: list[str] = []
    absent: list[AbsentCleanExpectation] = []
    for owner in plan.owners:
        parent = _path(plan, owner.parent_source, f"owner {owner.key} parent")
        source = _path(plan, owner.owner_source, f"owner {owner.key} source")
        count = _module_count(parent, owner.module_ident)
        static_family = _family_ids(set(inventory.tests), owner.owner_family)
        manifest_family = _family_ids(manifest_ids, owner.owner_family)
        expected = set(owner.ids)
        relevant_duplicates = {item[0] for item in inventory.duplicate_tests if item[0] in expected}
        if count == 0 and owner.policy == "optional":
            if source.exists() or static_family or manifest_family:
                _fail(f"optional owner {owner.key} is absent with residue")
            absent.append(AbsentCleanExpectation(owner.key))
            continue
        if count != 1 or not source.is_file():
            _fail(f"owner {owner.key} is not active exactly once")
        if static_family != expected or manifest_family != expected or relevant_duplicates:
            _fail(f"owner {owner.key} inventory/manifest closure differs from its immutable IDs")
        for test_id in owner.ids:
            site = inventory.tests.get(test_id, "").rsplit(":", 1)[0]
            if site != owner.owner_source:
                _fail(f"owner {owner.key} source site differs for {test_id}: {site}")
        candidates.extend(owner.ids)
    if _plan_digest(plan) != before:
        _fail("immutable proof plan changed during validation")
    execution_ids = tuple(candidates)
    if any(execution_ids is owner.ids for owner in plan.owners):
        _fail("execution tuple must not alias owner state")
    return SealedProof(execution_ids, tuple(absent), _identity(plan))


def _reduce(test_id: str, rc: int, output: str, prefix: str) -> None:
    lines = output.splitlines()
    reserved = (f"{prefix} PASS", f"{prefix} RESULT")
    if any(line.startswith(reserved) for line in lines):
        _fail(f"child output used a reserved parent record for {test_id}")
    headers = [line for line in lines if re.fullmatch(r"running [0-9]+ tests?", line)]
    results = [line for line in lines if line.startswith("test result:")]
    failures = [line for line in lines if line == "failures:" or line.endswith(" FAILED")]
    if rc != 0 or headers != ["running 1 test"] or len(results) != 1 or not RESULT.fullmatch(results[0]) or failures:
        _fail(f"{test_id} failed exact reducer: rc={rc} headers={headers} results={results}")


def run(plan: ProofPlan) -> ProofResult:
    """Execute with point-in-time governed-identity checks.

    Identity is checked at seal and immediately before and after every credited
    child. On the applicable path, it is checked once more after transcript
    normalization, reduction, and result construction but before proof records
    are emitted. The CI runner checkout is the trusted boundary. A
    governed-source change and restore wholly within one child window is not
    detected and is outside this engine's guarantee.
    """
    digest = _plan_digest(plan)
    sealed = seal(plan)
    if _plan_digest(plan) != digest:
        _fail("immutable proof plan changed while sealing")
    if not sealed.execution_ids:
        result = ProofResult(False, (), 0, 0)
        print(f"{plan.pass_prefix} NOT_APPLICABLE")
        for expectation in sealed.absent:
            print(expectation.render(result))
        return result
    transcripts: list[str] = []
    pass_records: list[str] = []
    for index, test_id in enumerate(sealed.execution_ids):
        if _identity(plan) != sealed.identity:
            _fail(f"sealed identity changed before credited child {test_id}")
        with tempfile.TemporaryDirectory(prefix=f"exact-rust-{index}-") as scratch:
            raw = Path(scratch) / "raw"
            normalized = Path(scratch) / "normalized"
            completed = subprocess.run(
                ["cargo", "test", "--lib", test_id, "--", "--exact", "--test-threads=1"],
                cwd=plan.repo_root,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                check=False,
            )
            if _identity(plan) != sealed.identity:
                _fail(f"sealed identity changed after credited child {test_id}")
            raw.write_bytes(completed.stdout)
            normalized.write_bytes(completed.stdout.replace(b"\r\n", b"\n").replace(b"\r", b"\n"))
            text = normalized.read_text("utf-8")
            _reduce(test_id, completed.returncode, text, plan.pass_prefix)
            transcripts.append(text)
            pass_records.append(f"{plan.pass_prefix} PASS id={test_id} selected=1 passed=1")
    result = ProofResult(True, sealed.execution_ids, len(sealed.execution_ids), len(pass_records))
    if _plan_digest(plan) != digest or result.execution_ids != sealed.execution_ids:
        _fail("proof plan or transactional execution tuple changed")
    if _identity(plan) != sealed.identity:
        _fail("sealed source/manifest/inventory identity changed during execution")
    for text in transcripts:
        print(text, end="" if text.endswith("\n") else "\n")
    for record in pass_records:
        print(record)
    print(f"{plan.pass_prefix} RESULT selected={result.selected} passed={result.passed}")
    for expectation in sealed.absent:
        print(expectation.render(result))
    return result


def parse_cli(argv: list[str] | None = None) -> ProofPlan:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    command = subparsers.add_parser("run")
    command.add_argument("--repo-root", required=True)
    command.add_argument("--manifest", required=True)
    command.add_argument("--pass-prefix", required=True)
    command.add_argument("--gate", nargs=5, required=True, metavar=("KEY", "PARENT", "MODULE", "CHILD", "POLICY"))
    command.add_argument("--owner", nargs=7, action="append", required=True, metavar=("KEY", "GATE", "PARENT", "MODULE", "SOURCE", "FAMILY", "POLICY"))
    command.add_argument("--owner-id", nargs=2, action="append", default=[], metavar=("OWNER", "ID"))
    args = parser.parse_args(argv)
    ids: dict[str, list[str]] = {}
    for owner_key, test_id in args.owner_id:
        ids.setdefault(owner_key, []).append(test_id)
    owners = tuple(OwnerSpec(*values, tuple(ids.pop(values[0], ()))) for values in args.owner)
    if ids:
        parser.error(f"owner IDs reference unknown keys: {', '.join(sorted(ids))}")
    return ProofPlan(Path(args.repo_root).resolve(), args.manifest, args.pass_prefix, GateSpec(*args.gate), owners)


def main(argv: list[str] | None = None) -> int:
    try:
        run(parse_cli(argv))
    except (OSError, UnicodeError, ProofError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
