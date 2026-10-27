#!/usr/bin/env python3
from __future__ import annotations
import hashlib
import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path
import generate_inventory_docs as inventory
from ratchet_admission import parse_cap_table
ROOT = Path(__file__).resolve().parent.parent
EVIDENCE = ROOT / "target/giant-file-progress/evidence.json"
REGISTRY = "scripts/giant_file_registry.toml"
EVALUATOR = "scripts/giant_file_progress.py"
METADATA = "scripts/giant_file_issue_metadata.json"
GIANT_PIN = "scripts/audit_maintainability_giant_baseline.toml"
TRANSITION = "scripts/giant_file_closed_issue_transition_list.txt"
LEDGER = frozenset({REGISTRY, METADATA, TRANSITION, GIANT_PIN})
def git(*args: str, binary: bool = False) -> str | bytes:
    result = subprocess.run(
        ["git", *args], cwd=ROOT, check=True, capture_output=True, text=not binary
    )
    return result.stdout
def oid(ref: str, suffix: str = "commit") -> str:
    return str(git("rev-parse", "--verify", f"{ref}^{{{suffix}}}" if suffix else ref)).strip()
# Bind evaluation to the immutable candidate and its ordered base/head parents.
def provenance_matches(candidate: str, base: str, head: str, parents: list[str]) -> bool: return parents == [candidate, base, head]

def pr_comparison_base(candidate: str, event_base: str, head: str,
                       parents: list[str]) -> str:
    """Resolve only a forward base advance on the event-provided candidate.

    GITHUB_SHA pins the candidate and the second parent must still be the
    event's exact head. A rewind, unrelated base, unavailable object, or Git
    error fails closed. No fetch or mutable branch lookup is involved.
    """
    mismatch = "event/base/head/merge object provenance mismatch"
    if len(parents) != 3 or not provenance_matches(candidate, parents[1], head, parents):
        raise RuntimeError(mismatch)
    comparison_base = parents[1]
    if comparison_base != event_base:
        try:
            git("merge-base", "--is-ancestor", event_base, comparison_base)
        except subprocess.CalledProcessError as error:
            if error.returncode == 1:
                raise RuntimeError(mismatch) from error
            raise  # Operational errors are not evidence of ancestry.
    return comparison_base

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
    additions = sum(int(row.split("\t", 1)[0]) for row in
                    str(git("diff", "--numstat", "--no-renames", base, candidate)).splitlines()
                    if row.split("\t", 1)[0].isdigit())
    return {"changed": changed, "additions": additions}

def base_pins(root: Path) -> dict[str, int]:
    """Read advisory references, rejecting malformed measurement inputs."""
    try:
        from audit_maintainability.checks.giant_file_ratchet import load_giant_baseline
        pins = parse_cap_table((root / GIANT_PIN).read_text(encoding="utf-8"),
                               "giant_file_ratchet")
        if not pins or load_giant_baseline(root / GIANT_PIN) != pins:
            raise ValueError("strict and audit giant pin maps differ or are empty")
        return pins
    except Exception as error:
        raise inventory.ParseError(f"invalid giant-file pins: {error}") from error

def giant_growth_warnings(base: dict[str, object],
                          candidate: dict[str, object]) -> list[str]:
    base_loc, candidate_loc = base["modules"], candidate["modules"]
    warnings = []
    for path, loc in sorted(candidate_loc.items()):
        old = base_loc.get(path, 0)
        raw, old_raw = candidate["raw_modules"][path], base["raw_modules"].get(path, 0)
        if loc >= 1000 and (old < 1000 or raw > old_raw):
            kind = "new giant" if old < 1000 else "giant growth"
            warnings.append(f"{kind}: {path}: {raw - old_raw:+d} raw lines "
                            f"({old_raw} -> {raw}); production LoC {old} -> {loc}")
    for path, cap in sorted(candidate.get("pins", {}).items()):
        old = base.get("pins", {}).get(path)
        if old is not None and cap > old:
            warnings.append(f"giant reference increased: {path}: +{cap - old} "
                            f"production lines ({old} -> {cap})")
    return warnings

def planning_warnings(base: dict[str, object], candidate: dict[str, object],
                      facts: dict[str, object]) -> list[str]:
    warnings = []
    changed = facts["changed"] & LEDGER
    if changed:
        warnings.append("giant planning ledger changed: " + ", ".join(sorted(changed)))
    if candidate["overdue"] != base["overdue"]:
        warnings.append("giant overdue planning set changed")
    shrunk = [path for path in base["overdue"]
              if 0 < base["modules"].get(path, 0) - candidate["modules"].get(path, 0) < 200]
    if shrunk:
        warnings.append("under 200-line giant progress: " + ", ".join(shrunk))
    if len(facts["changed"]) > 20 or facts["additions"] > 800:
        warnings.append("giant planning diff exceeds 20 files or 800 additions")
    return warnings

def pr_evaluation(base: dict[str, object], candidate: dict[str, object],
                  facts: dict[str, object]) -> tuple[str, list[str]]:
    return "pr_giant_warnings", giant_growth_warnings(base, candidate) + planning_warnings(base, candidate, facts)

def main_record(snapshot: dict[str, object]) -> dict[str, object]:
    overdue = snapshot["overdue"]
    return {"overdue": overdue, "overdue_count": len(overdue)}

def write_evidence(payload: dict[str, object]) -> None:
    EVIDENCE.parent.mkdir(parents=True, exist_ok=True)
    temporary = EVIDENCE.with_suffix(".tmp")
    temporary.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(EVIDENCE)

def emit_warnings(warnings: list[str]) -> None:
    for warning in warnings:
        print(f"WARNING: {warning}", file=sys.stderr)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary and warnings:
        with open(summary, "a", encoding="utf-8") as stream:
            stream.write("\n### Giant-file warnings\n\n")
            stream.writelines(f"- {warning}\n" for warning in warnings)

def main() -> int:
    env = os.environ
    event, repository = env.get("GFP_EVENT_NAME", ""), env.get("GFP_REPOSITORY", "")
    candidate_sha = env.get("GFP_CANDIDATE_SHA", "")
    selector, candidate = "main_giant_record", {"overdue": [], "registrations": {}}
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
        if event == "workflow_dispatch":
            selector = "reject"
            if (repository != "itismyfield/AgentDesk" or env.get("GFP_REF") != "refs/heads/main"
                    or env.get("GFP_REF_PROTECTED") != "true"
                    or candidate_sha != oid("refs/remotes/origin/main")):
                raise RuntimeError("dispatch requires protected main head in itismyfield/AgentDesk")
            selector = "main_giant_record"
            payload.update({"ref": env["GFP_REF"], "ref_protected": True,
                            "main_head_sha": candidate_sha})
        if event == "pull_request":
            selector = "pr_giant_warnings"
            # Fork PRs use the same immutable candidate provenance checks.
            if repository != "itismyfield/AgentDesk" or not env.get("GFP_HEAD_REPOSITORY"):
                raise RuntimeError(
                    "progress requires a pull request targeting itismyfield/AgentDesk "
                    "with a resolved head repository"
                )
            event_base_sha = oid(env.get("GFP_BASE_SHA", ""))
            head_sha = oid(env.get("GFP_HEAD_SHA", ""))
            parents = str(git("rev-list", "--parents", "-n1", candidate_sha)).split()
            payload.update({"event_base_sha": event_base_sha, "head_sha": head_sha,
                            "merge_sha": candidate_sha, "candidate_parents": parents[1:]})
            base_sha = pr_comparison_base(candidate_sha, event_base_sha, head_sha, parents)
            payload.update({"comparison_base_sha": base_sha, "merge_first_parent": base_sha})
        # Reject malformed provenance before archiving or inventory scanning.
        today = inventory.today_utc()
        with tempfile.TemporaryDirectory() as temporary:
            candidate_root = Path(temporary) / "candidate"
            candidate_root.mkdir(); archive(candidate_sha, candidate_root)
            candidate = inventory.giant_file_snapshot(candidate_root, evaluation_date=today)
            for path in candidate["overdue"]:
                _, _, deadline, issue, _ = candidate["registrations"][path]
                print(inventory.giant_deadline_warning(path, deadline, issue), file=sys.stderr)
            if event == "pull_request":
                base_root = Path(temporary) / "base"
                base_root.mkdir(); archive(base_sha, base_root)
                base = inventory.giant_file_snapshot(base_root, evaluation_date=today)
                base["pins"] = base_pins(base_root)
                candidate["pins"] = base_pins(candidate_root)
                facts = diff_facts(base_sha, candidate_sha)
                selector, warnings = pr_evaluation(base, candidate, facts)
                payload["warnings"] = warnings
                emit_warnings(warnings)
                base_evaluator = base_root / EVALUATOR
                base_evaluator_sha256 = (hashlib.sha256(base_evaluator.read_bytes()).hexdigest()
                                         if base_evaluator.exists() else "")
                candidate_evaluator_sha256 = hashlib.sha256(
                    (candidate_root / EVALUATOR).read_bytes()).hexdigest()
                payload.update({"event_base_sha": event_base_sha,
                    "merge_first_parent": parents[1], "head_sha": head_sha, "merge_sha": candidate_sha,
                    "base_tree": oid(base_sha, "tree"), "base_overdue": base["overdue"],
                    "retired": [], "changed_files": len(facts["changed"]),
                    "additions": facts["additions"]})
                if EVALUATOR in facts["changed"]:
                    payload.update({"evaluator_changed": True,
                        "base_evaluator_sha256": base_evaluator_sha256,
                        "candidate_evaluator_sha256": candidate_evaluator_sha256})
                reason = "giant growth and planning metadata are advisory"
            elif event in {"push", "workflow_dispatch"} and repository == "itismyfield/AgentDesk":
                selector = "main_giant_record"
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
    except Exception as error:
        payload.update({"selector": selector, "selected": selector != "reject", "candidate_overdue": candidate["overdue"],
            "ordinary_problem_count": 1, "verdict": "fail", "reason": str(error)})
        try:
            write_evidence(payload)
        except OSError as write_error:
            print(f"giant progress evidence write failed: {write_error}", file=sys.stderr)
        print(f"giant progress failed: {error}", file=sys.stderr)
        return 2
if __name__ == "__main__": raise SystemExit(main())
