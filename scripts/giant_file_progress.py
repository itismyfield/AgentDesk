#!/usr/bin/env python3
from __future__ import annotations
import hashlib
import io
import json
import os
import re
import subprocess
import sys
import tarfile
import tempfile
from collections import Counter
from pathlib import Path
import generate_inventory_docs as inventory
ROOT = Path(__file__).resolve().parent.parent
EVIDENCE = ROOT / "target/giant-file-progress/evidence.json"
REGISTRY = "scripts/giant_file_registry.toml"
EVALUATOR = "scripts/giant_file_progress.py"
METADATA = "scripts/giant_file_issue_metadata.json"
GENERATED_DOCS = frozenset({"ARCHITECTURE.md", "docs/generated/route-inventory.md",
                            "docs/generated/worker-inventory.md"})
GUARD_REPIN_ALLOWED = frozenset({
    "scripts/check_delivery_journal_raw_writer.py",
    "scripts/check_durable_frontier_writer_call_sites.py",
    "scripts/run_relay_authority_mutations.sh",
    "scripts/relay_authority_contract_targets.json",
})
FROZEN = ("scripts/audit_maintainability_giant_baseline.toml", "scripts/giant_file_closed_issue_transition_list.txt")
BOOTSTRAP_PATHS = frozenset({
    ".github/workflows/ci-main.yml",
    ".github/workflows/ci-pr.yml",
    "scripts/check-ci-runner-hardening.sh",
    "scripts/check_agent_maintenance_docs.py",
    "scripts/ci-script-checks.sh",
    "scripts/generate_inventory_docs.py",
    "scripts/giant_file_progress.py",
    REGISTRY,
    "src/services/discord/turn_finalizer.rs",
    "src/services/discord/turn_finalizer/terminal_handler.rs",
    "tests/test_api_docs_coverage.py",
    "tests/test_fast_check_ci_wiring.py",
    "tests/test_giant_file_progress.py",
    "tests/test_inventory_giant_split.py", "ARCHITECTURE.md",
    "docs/agent-maintenance/change-surfaces.md",
})
def git(*args: str, binary: bool = False) -> str | bytes:
    result = subprocess.run(
        ["git", *args], cwd=ROOT, check=True, capture_output=True, text=not binary
    )
    return result.stdout
def oid(ref: str, suffix: str = "commit") -> str:
    return str(git("rev-parse", "--verify", f"{ref}^{{{suffix}}}" if suffix else ref)).strip()
def provenance_matches(candidate: str, base: str, head: str, origin: str, parents: list[str]) -> bool: return parents == [candidate, base, head] and base == origin
def archive(ref: str, destination: Path) -> None:
    with tarfile.open(fileobj=io.BytesIO(git("archive", "--format=tar", ref, binary=True))) as bundle:
        members = bundle.getmembers()
        unsafe = any(item.name.startswith("/") or ".." in Path(item.name).parts
                     or not (item.isfile() or item.isdir()) for item in members)
        if unsafe:
            raise RuntimeError("snapshot contains a non-regular or unsafe path")
        bundle.extractall(destination)
def diff_facts(base: str, candidate: str) -> dict[str, object]:
    changed = set(str(git("diff", "--name-only", "-z", base, candidate)).split("\0")) - {""}
    additions, numstat, binary = 0, {}, set()
    for row in str(git("diff", "--numstat", "--no-renames", base, candidate)).splitlines():
        added, deleted, path = row.split("\t", 2)
        if not added.isdigit() or not deleted.isdigit():
            binary.add(path)
            continue
        numstat[path] = (int(added), int(deleted))
        additions += int(added)
    status_rows = str(git("diff", "--name-status", "--no-renames", base, candidate)).splitlines()
    statuses = {row.split("\t", 1)[1]: row.split("\t", 1)[0] for row in status_rows}
    rename_copy = str(git("diff", "--name-status", "--find-renames", "--find-copies", base, candidate))
    return {"changed": changed, "additions": additions, "numstat": numstat,
            "binary": binary, "statuses": statuses,
            "rename_copy": any(row.startswith(("R", "C")) for row in rename_copy.splitlines())}
_GUARD_HUNK_RE = re.compile(rb"^@@ -\d+(?:,(?P<old_count>\d+))? \+\d+(?:,(?P<new_count>\d+))? @@(?: .*)?$")

def guard_repin_candidate(path: str, facts: dict[str, object]) -> bool:
    stat = facts["numstat"].get(path)
    return (path in GUARD_REPIN_ALLOWED and facts["statuses"].get(path) == "M"
            and path not in facts.get("binary", set()) and stat is not None
            and stat[0] == stat[1] and stat[0] <= 8)

def guard_repin_candidates(paths: set[str], facts: dict[str, object]) -> set[str]:
    try:
        return {path for path in paths if guard_repin_candidate(path, facts)}
    except Exception as error:
        raise RuntimeError("guard repin candidate classification failed") from error

def guard_repin_patches(base: str, candidate: str,
                        facts: dict[str, object]) -> dict[str, bytes]:
    try:
        patches = {}
        for path in sorted(guard_repin_candidates(facts["changed"], facts)):
            patch = git("diff", "-U0", "--no-renames", base, candidate,
                        "--", path, binary=True)
            if not isinstance(patch, bytes):
                raise TypeError("binary Git wrapper returned text")
            patches[path] = patch
        return patches
    except Exception as error:
        raise RuntimeError("guard repin patch capture failed") from error

def whole_quoted_literal_pattern(root: bytes) -> re.Pattern[bytes]:
    return re.compile(rb'(?P<q>["\'])' + re.escape(root) + rb'(?P=q)')

def _guard_repin_pairs(patch: bytes) -> list[tuple[bytes, bytes]] | None:
    lines = patch.split(b"\n")
    if lines and lines[-1] == b"":
        lines.pop()
    first = next((index for index, line in enumerate(lines)
                  if line.startswith(b"@@ ")), -1)
    if first < 0 or len(lines[:first]) != 4:
        return None
    prefix = lines[:first]
    if not (prefix[0].startswith(b"diff --git ")
            and prefix[1].startswith(b"index ")
            and prefix[2].startswith(b"--- ")
            and prefix[3].startswith(b"+++ ")):
        return None
    blocks, current, phase = [], None, "removed"
    for line in lines[first:]:
        match = _GUARD_HUNK_RE.match(line)
        if match:
            if current is not None:
                blocks.append(current)
            current = [int(match.group("old_count") or b"1"),
                       int(match.group("new_count") or b"1"), [], []]
            phase = "removed"
        elif current is None:
            return None
        elif line.startswith(b"-"):
            if phase == "added":
                return None
            current[2].append(line[1:])
        elif line.startswith(b"+"):
            phase = "added"
            current[3].append(line[1:])
        else:
            return None
    if current is not None:
        blocks.append(current)
    pairs = []
    for old_count, new_count, removed, added in blocks:
        if (old_count < 1 or old_count != new_count
                or len(removed) != old_count or len(added) != new_count):
            return None
        pairs.extend(zip(removed, added))
    return pairs or None

def pure_guard_repin(patch: bytes, root: bytes, child: bytes) -> bool:
    try:
        pairs = _guard_repin_pairs(patch)
        if not pairs or not root or not child:
            return False
        pattern = whole_quoted_literal_pattern(root)
        for removed, added in pairs:
            rewritten, count = pattern.subn(
                lambda match: match.group("q") + child + match.group("q"), removed)
            if count < 1 or added != rewritten or root in added:
                return False
        return True
    except Exception as error:
        raise RuntimeError("guard repin byte proof failed") from error

def matching_guard_repin_destinations(
        path: str, patch: bytes, retired: set[str],
        children: dict[str, list[str]], statuses: dict[str, str],
        candidate_loc: dict[str, int], registered_giant_roots: set[str],
) -> list[tuple[str, str]]:
    try:
        matches = []
        for root in sorted(retired):
            for child in sorted(set(children.get(root, ()))):
                if (statuses.get(child) != "A" or child in retired
                        or child in registered_giant_roots
                        or child not in candidate_loc or candidate_loc[child] >= 1000):
                    continue
                if pure_guard_repin(patch, root.encode(), child.encode()):
                    matches.append((root, child))
        return matches
    except Exception as error:
        raise RuntimeError(f"guard repin proof failed: {path}") from error
_HUNK_RE = re.compile(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@")

def production_line_numbers(text: str, production_loc: int) -> set[int]:
    if production_loc == 0:
        return set()
    test_lines: set[int] = set()
    for match in inventory._CFG_MOD_RE.finditer(text):
        if not inventory.cfg_requires_test(match.group("predicate")):
            continue
        brace = text.rindex("{", match.start(), match.end())
        try:
            _body, end = inventory.scan_balanced(text, brace, "{", "}")
        except inventory.ParseError:
            continue
        test_lines.update(range(inventory.offset_to_line(text, match.start()),
                                inventory.offset_to_line(text, end) + 1))
    return set(range(1, inventory.line_count(text) + 1)) - test_lines

def movement_ledger(base: str, candidate: str, progress_roots: set[str],
                    children: dict[str, list[str]], base_loc: dict[str, int],
                    candidate_loc: dict[str, int]) -> dict[str, tuple[tuple[str, int], ...]]:
    roots = sorted(progress_roots)
    if not roots:
        return {}
    paths = sorted(set(roots) | {child for root in roots for child in children.get(root, ())})
    patch = str(git("diff", "--unified=0", "--no-renames", base, candidate,
                    "--", *paths))
    production: dict[tuple[str, str], set[int]] = {}
    for ref, locations, selected in ((base, base_loc, roots),
                                     (candidate, candidate_loc, paths)):
        for path in selected:
            loc = locations.get(path, 0)
            if loc:
                text = str(git("show", f"{ref}:{path}"))
                production[(ref, path)] = production_line_numbers(text, loc)
            else:
                production[(ref, path)] = set()

    deleted = {root: Counter() for root in roots}
    added: list[tuple[str, int, str]] = []
    old_path = new_path = ""
    old_line = new_line = 0
    for line in patch.splitlines():
        if line.startswith("--- "):
            old_path = "" if line == "--- /dev/null" else line[6:]
        elif line.startswith("+++ "):
            new_path = "" if line == "+++ /dev/null" else line[6:]
        elif match := _HUNK_RE.match(line):
            old_line, new_line = int(match.group(1)), int(match.group(2))
        elif line.startswith("-") and old_path:
            value = line[1:].strip()
            if (old_path in deleted and old_line in production[(base, old_path)]
                    and value and not value.startswith(("//", "/*", "*"))):
                deleted[old_path][value] += 1
            old_line += 1
        elif line.startswith("+") and new_path:
            value = line[1:].strip()
            if (new_line in production[(candidate, new_path)]
                    and value and not value.startswith(("//", "/*", "*"))):
                added.append((new_path, new_line, value))
            new_line += 1
        elif line.startswith(" "):
            old_line += 1
            new_line += 1

    ledger: dict[str, list[tuple[str, int]]] = {root: [] for root in roots}
    for path, line, value in sorted(added):
        for root in roots:
            if path in children.get(root, ()) and deleted[root][value]:
                ledger[root].append((path, line))
                deleted[root][value] -= 1
                break
    return {root: tuple(occurrences) for root, occurrences in ledger.items()}
def without_entry(text: str, path: str) -> str | None:
    lines = text.splitlines(keepends=True)
    try:
        file_line = next(i for i, line in enumerate(lines) if line.strip() == f'file = "{path}"')
        start = max(i for i in range(file_line + 1) if lines[i].strip() == "[[entry]]")
        end = next(i for i in range(file_line + 1, len(lines)) if not lines[i].strip())
    except (StopIteration, ValueError):
        return None
    return "".join(lines[:start] + lines[end + 1:])
def new_or_growing_errors(base: dict[str, object],
                          candidate: dict[str, object]) -> list[str]:
    base_loc, candidate_loc = base["modules"], candidate["modules"]
    return [f"new or growing giant: {path}" for path, loc in candidate_loc.items()
            if loc >= 1000 and (base_loc.get(path, 0) < 1000
                                or loc > base_loc.get(path, 0))]
def ordinary_no_regression_errors(base: dict[str, object], candidate: dict[str, object],
                                  facts: dict[str, object]) -> list[str]:
    errors: list[str] = []
    if candidate["overdue"] != base["overdue"]:
        errors.append("ordinary PR changed overdue debt")
    errors.extend(new_or_growing_errors(base, candidate))
    if not facts["authority_equal"]:
        errors.append("frozen authority blob changed")
    if not facts["registry_equal"]:
        errors.append("registry changed in ordinary no-regression PR")
    return errors

def progress_errors(base: dict[str, object], candidate: dict[str, object],
                    facts: dict[str, object]) -> list[str]:
    errors: list[str] = []
    before, after = set(base["overdue"]), set(candidate["overdue"])
    base_loc, candidate_loc = base["modules"], candidate["modules"]
    base_meta, candidate_meta = base["registrations"], candidate["registrations"]
    retired = before - after
    partial = not retired and before == after
    shrunk = {path for path in before if candidate_loc.get(path, 0) <= base_loc.get(path, 0) - 200}
    progress_roots = retired or shrunk
    if not before or not (after < before or (partial and shrunk)):
        errors.append("PR has neither retirement nor 200-line partial progress")
    if facts["rename_copy"]:
        errors.append("rename/copy cannot prove same-path progress")
    changed = facts["changed"]
    moved_union = {occurrence for occurrences in facts["moved"].values()
                   for occurrence in occurrences}
    if len(changed) > 20 or facts["additions"] - len(moved_union) > 800:
        errors.append("diff exceeds 20 files or 800 non-moved additions")
    expected = BOOTSTRAP_PATHS if facts["bootstrap"] else {
        *(retired and {REGISTRY} or set()), *progress_roots,
        *(child for children in facts["children"].values() for child in children),
    }
    optional_metadata = {METADATA} if METADATA in changed else set()
    extras = changed - expected - optional_metadata
    test_extras = {path for path in extras
        if path.startswith("tests/test_") and path.endswith(".py")
        and "/" not in path[len("tests/"): -len(".py")]
        and path not in {"tests/test_giant_file_progress.py",
                         "tests/test_inventory_giant_split.py"}
        and facts["statuses"].get(path) == "M"
        and path not in facts.get("binary", set())
        and max(facts["numstat"].get(path, (9, 9))) <= 8}
    generated_extras = {path for path in extras
        if path in GENERATED_DOCS and facts["statuses"].get(path) == "M"
        and path not in facts.get("binary", set())}
    maintenance_extras = {path for path in extras
        if path.startswith("docs/agent-maintenance/") and path.endswith(".md")
        and facts["statuses"].get(path) == "M"
        and path not in facts.get("binary", set())
        and max(facts["numstat"].get(path, (41, 41))) <= 40}
    guard_candidates = (guard_repin_candidates(extras, facts)
                        if retired and not facts["bootstrap"] else set())
    guard_extras: set[str] = set()
    registered_giant_roots = set(base_meta) | set(candidate_meta)
    for path in sorted(guard_candidates):
        patch = facts.get("guard_repin_patches", {}).get(path, b"")
        destinations = matching_guard_repin_destinations(
            path, patch, retired, facts["children"], facts["statuses"],
            candidate_loc, registered_giant_roots)
        if len(destinations) == 1:
            guard_extras.add(path)
        else:
            errors.append(
                f"guard repin is not a pure root→child path substitution: {path}")
    allowed_extras = test_extras | generated_extras | maintenance_extras | guard_extras
    if len(test_extras) > 3:
        errors.append("progress has more than 3 pin re-derivation files")
    if len(maintenance_extras) > 3:
        errors.append("progress has more than 3 maintenance documentation files")
    if not expected <= changed or extras != allowed_extras:
        errors.append("changed-path closure is not exact")
    for path in after:
        if base_meta.get(path) != candidate_meta.get(path):
            errors.append(f"retained metadata changed: {path}")
    errors.extend(new_or_growing_errors(base, candidate))
    for path in progress_roots:
        old, new = base_loc.get(path), candidate_loc.get(path)
        children = facts["children"].get(path, ())
        if old is None or old < 1000 or new is None or new >= old:
            errors.append(f"not actual same-path progress: {path}")
        if path in retired and (new is None or new >= 1000
                                or path not in base_meta or path in candidate_meta):
            errors.append(f"registry entry not retired exactly once: {path}")
        if not children or any(candidate_loc.get(child, 0) >= 1000 for child in children):
            errors.append(f"progress lacks bounded derived child: {path}")
        if len(facts["moved"].get(path, ())) < max(1, min(20, (old or 0) - (new or 0))):
            errors.append(f"progress lacks moved production code: {path}")
    if not facts["authority_equal"]:
        errors.append("frozen authority blob changed")
    if retired and not facts["registry_exact"]:
        errors.append("registry is not the exact retired-entry deletion")
    if partial and not facts["registry_equal"]:
        errors.append("registry changed during partial progress")
    return errors

def pr_evaluation(base: dict[str, object], candidate: dict[str, object],
                  facts: dict[str, object]) -> tuple[str, list[str]]:
    before, after = set(base["overdue"]), set(candidate["overdue"])
    if after < before:
        return "pr_strict_progress", progress_errors(base, candidate, facts)
    shrink_attempt = (before == after and any(
        (new := candidate["modules"].get(path)) is not None
        and new < base["modules"].get(path, 0)
        for path in before))
    if shrink_attempt:
        return "pr_strict_progress", progress_errors(base, candidate, facts)
    return "pr_ordinary_no_regression", ordinary_no_regression_errors(base, candidate, facts)

def main_record(snapshot: dict[str, object]) -> dict[str, object]:
    overdue = snapshot["overdue"]
    return {"overdue": overdue, "overdue_count": len(overdue)}

def write_evidence(payload: dict[str, object]) -> None:
    EVIDENCE.parent.mkdir(parents=True, exist_ok=True)
    temporary = EVIDENCE.with_suffix(".tmp")
    temporary.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(EVIDENCE)
def main() -> int:
    env = os.environ
    event, repository = env.get("GFP_EVENT_NAME", ""), env.get("GFP_REPOSITORY", "")
    candidate_sha = env.get("GFP_CANDIDATE_SHA", "")
    selector, candidate = "main_no_regression_record", {"overdue": [], "registrations": {}}
    payload: dict[str, object] = {
        "schema": 1, "evaluator_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "event": event, "repository": repository, "selected": True, "executed": True,
    }
    try:
        if Path(__file__).is_symlink() or str(git("status", "--porcelain")).strip():
            raise RuntimeError("evaluator input must be regular and clean")
        candidate_sha = oid(candidate_sha)
        if oid("HEAD") != candidate_sha:
            raise RuntimeError("candidate SHA is not checked-out HEAD")
        today = inventory.today_utc()
        with tempfile.TemporaryDirectory() as temporary:
            candidate_root = Path(temporary) / "candidate"
            candidate_root.mkdir(); archive(candidate_sha, candidate_root)
            candidate = inventory.giant_file_snapshot(candidate_root, evaluation_date=today)
            if event == "pull_request":
                selector = "pr_strict_progress"
                if repository != "itismyfield/AgentDesk" or env.get("GFP_HEAD_REPOSITORY") != repository:
                    raise RuntimeError("progress requires an exact same-repository PR")
                git("fetch", "--no-tags", "origin", "+refs/heads/main:refs/remotes/origin/main")
                base_sha, head_sha = oid(env.get("GFP_BASE_SHA", "")), oid(env.get("GFP_HEAD_SHA", ""))
                parents = str(git("rev-list", "--parents", "-n1", candidate_sha)).split()
                origin_sha = oid("origin/main")
                if not provenance_matches(candidate_sha, base_sha, head_sha, origin_sha, parents):
                    raise RuntimeError("event/base/head/merge object provenance mismatch")
                base_root = Path(temporary) / "base"
                base_root.mkdir(); archive(base_sha, base_root)
                base = inventory.giant_file_snapshot(base_root, evaluation_date=today)
                facts = diff_facts(base_sha, candidate_sha)
                facts["bootstrap"] = not (base_root / EVALUATOR).exists()
                before, after = set(base["overdue"]), set(candidate["overdue"])
                retired = before - after
                shrunk = {path for path in before if candidate["modules"].get(path, 0) <= base["modules"].get(path, 0) - 200}
                progress_roots = retired or shrunk
                facts["children"] = {path: sorted(child for child in facts["changed"]
                    if child.startswith(path[:-3] + "/") and child.endswith(".rs")) for path in progress_roots}
                facts["guard_repin_patches"] = (guard_repin_patches(
                    base_sha, candidate_sha, facts)
                    if retired and not facts["bootstrap"] else {})
                facts["moved"] = movement_ledger(
                    base_sha, candidate_sha, progress_roots, facts["children"],
                    base["modules"], candidate["modules"])
                facts["authority_equal"] = all(oid(f"{base_sha}:{path}", "")
                    == oid(f"{candidate_sha}:{path}", "") for path in FROZEN)
                facts["registry_equal"] = oid(f"{base_sha}:{REGISTRY}", "") == oid(
                    f"{candidate_sha}:{REGISTRY}", "")
                base_evaluator = base_root / EVALUATOR
                base_evaluator_sha256 = (hashlib.sha256(base_evaluator.read_bytes()).hexdigest()
                                         if base_evaluator.exists() else "")
                candidate_evaluator_sha256 = hashlib.sha256(
                    (candidate_root / EVALUATOR).read_bytes()).hexdigest()
                expected = (base_root / REGISTRY).read_text(encoding="utf-8")
                for path in sorted(retired):
                    expected = without_entry(expected, path) or ""
                facts["registry_exact"] = expected == (candidate_root / REGISTRY).read_text(encoding="utf-8")
                selector, errors = pr_evaluation(base, candidate, facts)
                if errors:
                    raise RuntimeError("; ".join(errors))
                payload.update({"event_base_sha": base_sha, "observed_origin_main_sha": origin_sha,
                    "merge_first_parent": parents[1], "head_sha": head_sha, "merge_sha": candidate_sha,
                    "base_tree": oid(base_sha, "tree"), "base_overdue": base["overdue"],
                    "retired": [{"path": path, "base_prod_loc": base["modules"][path], "candidate_prod_loc": candidate["modules"][path], "children": [{"path": child, "candidate_prod_loc": candidate["modules"][child]} for child in facts["children"][path]]} for path in sorted(retired)],
                    "changed_files": len(facts["changed"]), "additions": facts["additions"]})
                if EVALUATOR in facts["changed"]:
                    payload.update({"evaluator_changed": True,
                        "base_evaluator_sha256": base_evaluator_sha256,
                        "candidate_evaluator_sha256": candidate_evaluator_sha256})
                reason = {
                    "pr_ordinary_no_regression": "ordinary PR preserves giant-file debt",
                    "pr_strict_progress": "retirement or 200-line partial progress",
                }[selector]
            elif event == "push" and repository == "itismyfield/AgentDesk":
                selector = "main_no_regression_record"
                payload.update(main_record(candidate))
                reason = "main records current giant-file debt"
            else:
                selector = "reject"
                raise RuntimeError("event is not protected main/push or same-repository PR")
            if env.get("GFP_REFRESH_DOCS") == "1":
                inventory.write_documents(inventory.generated_documents(allow_overdue=True), check=False)
            payload.update({"selector": selector, "candidate_tree": oid(candidate_sha, "tree"),
                "candidate_overdue": candidate["overdue"], "ordinary_problem_count": 0,
                "metadata_fingerprints": {path: hashlib.sha256(repr(value).encode()).hexdigest()
                    for path, value in sorted(candidate["registrations"].items())},
                "verdict": "progress-pass", "reason": reason})
            write_evidence(payload)
            return 0
    except (OSError, RuntimeError, subprocess.CalledProcessError, inventory.ParseError) as error:
        payload.update({"selector": selector, "selected": selector != "reject", "candidate_overdue": candidate["overdue"],
            "ordinary_problem_count": 1, "verdict": "fail", "reason": str(error)})
        try:
            write_evidence(payload)
        except OSError as write_error:
            print(f"giant progress evidence write failed: {write_error}", file=sys.stderr)
        print(f"giant progress failed: {error}", file=sys.stderr)
        return 2
if __name__ == "__main__": raise SystemExit(main())
