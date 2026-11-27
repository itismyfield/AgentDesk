"""Keep new Rust test files from splitting an existing directory's tests."""

from pathlib import PurePosixPath
import subprocess


def is_rust_source(path):
    return (path.endswith(".rs") and path.startswith(("src/", "tests/", "crates/", "examples/", "benches/"))
            and "/fixtures/" not in path)


def _git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True, encoding="utf-8").stdout


def _existing_tests(base, candidates, has_test_module):
    for path in candidates:
        if path.name == "tests.rs" or path.name.endswith(("_tests.rs", "_test.rs")):
            return path
    for path in candidates:
        if has_test_module(_git("show", f"{base}:{path}")):
            return path
    return None


def audit_test_files(base, has_test_module):
    if base is None:
        return []
    added = _git("diff", "--name-only", "--no-ext-diff", "--no-renames", "--diff-filter=A", "-z", base, "--", "*.rs")
    untracked = _git("ls-files", "--others", "--exclude-standard", "-z", "--", "*.rs")
    files = sorted({PurePosixPath(path) for path in (added + untracked).split("\0")
                    if path.endswith("_tests.rs") and is_rust_source(path)})
    if not files:
        return []
    siblings = {}
    for name in _git("ls-tree", "-r", "--name-only", "-z", base).split("\0"):
        if is_rust_source(name):
            path = PurePosixPath(name)
            siblings.setdefault(path.parent, []).append(path)

    existing = {}
    findings = []
    for path in files:
        directory = path.parent
        if directory not in existing:
            candidates = siblings.get(directory, []) + siblings.get(directory / "tests", [])
            if directory.name:
                owner = directory.with_name(f"{directory.name}.rs")
                if owner in siblings.get(owner.parent, []):
                    candidates.append(owner)
            existing[directory] = _existing_tests(base, candidates, has_test_module)
        if existing[directory]:
            findings.append(f"{path}: add cases to existing tests in {existing[directory]} "
                            "(docs/agent-maintenance/test-policy.md rule 14)")
    return findings
