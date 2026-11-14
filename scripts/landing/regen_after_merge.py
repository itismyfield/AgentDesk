#!/usr/bin/env python3
"""Regenerate merged documentation, preserving manual text and recording review evidence."""
from __future__ import annotations

import argparse
import difflib
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

from merge_autoresolve import generated_block

BLOCKS = {
    "ARCHITECTURE.md": ("SRC TREE", "TOP LEVEL MODULE MAP"),
    "docs/agent-maintenance/change-surfaces.md": ("GIANT PATHS",),
}
WHOLE_FILES = (
    "docs/generated/route-inventory.md", "docs/generated/worker-inventory.md",
    "docs/generated/env-reference.md", "docs/generated/cli-reference.md",
)
MIXED_JSON = {"docs/e2e-continuous/bundles.schema1.json": ("scenario_catalog",)}
OUTPUTS = (*BLOCKS, *WHOLE_FILES, *MIXED_JSON)
COMMANDS = (
    ("scripts/generate_inventory_docs.py", "--review-docs-only", "--sync-giant-docs"),
    ("scripts/generate_env_reference.py",),
    ("scripts/generate_cli_reference.py",),
    ("scripts/e2e/generate_continuous_catalog.py", "--write"),
)
CONFLICT = re.compile(rb"^(?:<{7,}|={7,}|>{7,}|\|{7,})(?: |\r?$)", re.MULTILINE)


def git(root: Path, *args: str) -> bytes:
    return subprocess.run(["git", "-C", str(root), *args], check=True,
                          capture_output=True).stdout


def manual_bytes(data: bytes, names: tuple[str, ...]) -> bytes:
    tokens = re.findall(rb"<!-- (?:BEGIN|END) GENERATED[^>]*-->", data)
    expected = {f"<!-- {edge} GENERATED: {name} -->".encode()
                for name in names for edge in ("BEGIN", "END")}
    if set(tokens) != expected:
        raise ValueError("missing or unknown generated markers")
    spans = []
    for name in names:
        start = f"<!-- BEGIN GENERATED: {name} -->".encode()
        end = f"<!-- END GENERATED: {name} -->".encode()
        if (data.count(start) != 1 or data.count(end) != 1
                or len(re.findall(rb"(?m)^" + re.escape(start) + rb"$", data)) != 1
                or len(re.findall(rb"(?m)^" + re.escape(end) + rb"$", data)) != 1):
            raise ValueError(f"expected exactly one marker pair: {name}")
        left, right = data.index(start) + len(start), data.index(end)
        if left > right:
            raise ValueError(f"reversed markers: {name}")
        spans.append((left, right))
    spans.sort()
    if any(right > next_left for (left, right), (next_left, next_right)
           in zip(spans, spans[1:])):
        raise ValueError("nested generated blocks")
    result, cursor = b"", 0
    for left, right in spans:
        result += data[cursor:left]
        cursor = right
    return result + data[cursor:]


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def json_manual_bytes(data: bytes, generated: tuple[str, ...]) -> bytes:
    def unique_object(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f"duplicate JSON key: {key}")
            result[key] = value
        return result

    value = json.loads(data, object_pairs_hook=unique_object)
    if not isinstance(value, dict) or any(not isinstance(value.get(key), dict) for key in generated):
        raise ValueError("missing or invalid generated JSON object")
    return json.dumps({key: item for key, item in value.items() if key not in generated},
                      sort_keys=True, ensure_ascii=False, allow_nan=False).encode()


def snapshot(root: Path) -> dict[str, bytes]:
    return {path: (root / path).read_bytes() if (root / path).exists() else b""
            for path in OUTPUTS}


def outside_hashes(root: Path) -> dict[str, str]:
    paths = git(root, "ls-files", "-z").decode().split("\0")
    return {path: digest((root / path).read_bytes()) for path in paths
            if path and path not in OUTPUTS and (root / path).is_file()}


def write_diff(path: Path, before: dict[str, bytes | None], after: dict[str, bytes]) -> None:
    with path.open("w", encoding="utf-8") as stream:
        for name in OUTPUTS:
            stream.writelines(difflib.unified_diff(
                (before[name] or b"").decode().splitlines(keepends=True),
                after[name].decode().splitlines(keepends=True),
                fromfile="/dev/null" if before[name] is None else f"a/{name}",
                tofile=f"b/{name}"))


def regenerate(root: Path, artifact: Path, check: bool, check_merge: bool = False) -> int:
    artifact.mkdir(parents=True, exist_ok=True)
    if any(artifact.iterdir()):
        raise ValueError("artifact directory must be empty (one directory per attempt)")
    before: dict[str, bytes] = {}
    report: dict[str, object] = {"status": "CHECK_FAIL", "commands": COMMANDS, "check": check,
                                 "check_merge": check_merge}
    result = 2
    try:
        if Path(git(root, "rev-parse", "--show-toplevel").decode().strip()).resolve() != root:
            raise ValueError("--worktree must name a git worktree root")
        report.update(head=git(root, "rev-parse", "HEAD").decode().strip(),
                      head_tree=git(root, "rev-parse", "HEAD^{tree}").decode().strip(),
                      parents=git(root, "show", "-s", "--format=%P", "HEAD").decode().split())
        unresolved = git(root, "diff", "--name-only", "--diff-filter=U", "-z").decode().split("\0")
        unresolved = [path for path in unresolved if path]
        report["raw_conflicts"] = unresolved
        if set(unresolved) - set(OUTPUTS):
            raise ValueError(f"manual/unowned conflict: {sorted(set(unresolved) - set(OUTPUTS))}")
        for name in OUTPUTS:
            if (root / name).resolve() != root / name:
                raise ValueError(f"generated output is a symlink: {name}")
        before = snapshot(root)
        manual = {name: manual_bytes(before[name], names) for name, names in BLOCKS.items()}
        json_manual = {name: json_manual_bytes(before[name], keys) for name, keys in MIXED_JSON.items()}
        resolved = {}
        for name in BLOCKS:
            lines = generated_block(before[name].decode().splitlines(keepends=True),
                                    "<!-- BEGIN GENERATED", "<!-- END GENERATED", "GENERATED")
            if lines is None:
                raise ValueError(f"manual region or marker conflict: {name}")
            resolved[name] = "".join(lines).encode()
            if CONFLICT.search(resolved[name]):
                raise ValueError(f"malformed conflict: {name}")
        changed = git(root, "diff", "--name-only", "HEAD", "-z").decode().split("\0")
        for name in changed:
            path = root / name
            if name and name not in OUTPUTS and path.is_file() and CONFLICT.search(path.read_bytes()):
                raise ValueError(f"manual conflict markers: {name}")
        outside = outside_hashes(root)
        for name, data in resolved.items():
            (root / name).write_bytes(data)
        with (artifact / "generator.log").open("w", encoding="utf-8") as log:
            for command in COMMANDS:
                log.write(f"$ {sys.executable} {' '.join(command)}\n")
                log.flush()
                subprocess.run([sys.executable, *command], cwd=root, stdout=log,
                               stderr=subprocess.STDOUT, check=True)
        after = snapshot(root)
        if any(manual_bytes(after[name], names) != manual[name] for name, names in BLOCKS.items()):
            raise ValueError("generator changed manual regions")
        if any(json_manual_bytes(after[name], keys) != json_manual[name] for name, keys in MIXED_JSON.items()):
            raise ValueError("generator changed manual JSON fields")
        if outside_hashes(root) != outside:
            raise ValueError("generator changed an unowned tracked path")
        if any(CONFLICT.search(data) for data in after.values()):
            raise ValueError("conflict markers remain after regeneration")
        tracked = set(git(root, "ls-tree", "-r", "--name-only", "-z", "HEAD", "--", *OUTPUTS).decode().split("\0"))
        head_before = {name: git(root, "show", f"HEAD:{name}") if name in tracked else None for name in OUTPUTS}
        report["head_missing_outputs"] = [name for name in OUTPUTS if head_before[name] is None]
        write_diff(artifact / "head.diff", head_before, after)
        if check and any(head_before[name] != after[name] for name in OUTPUTS):
            report["status"] = "DRIFT"
            result = 1
        else:
            if unresolved and not check and not check_merge:
                git(root, "add", "--", *unresolved)
            report["status"] = "REGEN_OK"
            result = 0
        report["manual_regions_preserved"] = True
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        report["error"] = str(error)
        print(f"regeneration failed: {error}", file=sys.stderr)
    finally:
        try:
            if before:
                after = snapshot(root)
                report["files"] = {
                    name: {"before_sha256": digest(before[name]), "after_sha256": digest(after[name]),
                           "added": sum(1 for line in difflib.ndiff(before[name].decode().splitlines(),
                                                                   after[name].decode().splitlines())
                                        if line.startswith("+ ")),
                           "removed": sum(1 for line in difflib.ndiff(before[name].decode().splitlines(),
                                                                     after[name].decode().splitlines())
                                          if line.startswith("- "))}
                    for name in OUTPUTS}
                write_diff(artifact / "regeneration.diff", before, after)
        except (OSError, ValueError) as error:
            report["evidence_errors"] = [str(error)]
            report["status"], result = "CHECK_FAIL", 2
        finally:
            if result == 2:
                for name, data in before.items():
                    try:
                        (root / name).write_bytes(data)
                    except OSError as error:
                        report.setdefault("restore_errors", {})[name] = str(error)
        report["exit_code"] = result
        try:
            (artifact / "manifest.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        except OSError as error:
            report["status"], result = "CHECK_FAIL", 2
            print(f"manifest write failed: {error}", file=sys.stderr)
    print(f"{report['status']} artifact={artifact}")
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worktree", type=Path, default=Path.cwd())
    parser.add_argument("--artifact-dir", type=Path, required=True)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true", help="exit 1 on generated diff against HEAD; do not stage")
    mode.add_argument("--check-merge", action="store_true",
                      help="validate an isolated merged worktree; preserve the index and record all regeneration diff")
    args = parser.parse_args()
    try:
        return regenerate(args.worktree.resolve(), args.artifact_dir.resolve(), args.check, args.check_merge)
    except (OSError, ValueError) as error:
        print(f"regeneration failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
