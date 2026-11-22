"""Choose the first-phase exact IDs for a PR's unfiltered nextest inventory."""

from dataclasses import dataclass
from pathlib import Path

from rust_module_scope import GitQueryError, MappingError, git, select_modules


SHARED_DIRS = (
    ".github/", ".cargo/", ".config/", "crates/", "tests/", "migrations/",
    "scripts/", "policies/", "assets/", "examples/", "src/bin/", "src/test_support/",
)
SHARED_NAMES = {"Cargo.toml", "Cargo.lock", "build.rs", "rust-toolchain.toml", "rust-toolchain"}
SHARED_PATHS = {
    "src/lib.rs", "src/main.rs", "src/test_support.rs", "defaults.json",
    "Makefile", "justfile", "package.json", "package-lock.json",
}


@dataclass(frozen=True)
class ScopeSelection:
    mode: str
    reason: str
    ids: frozenset[tuple[str, str]]


def _shared_input(path):
    return (path.startswith(SHARED_DIRS) or Path(path).name in SHARED_NAMES or path in SHARED_PATHS
            or path.startswith("src/") and not path.endswith(".rs"))


def select_scope(repo, base_sha, head_sha, head_ref, runnable_ids, *, core_ids=()):
    """Use an exact merge checkout; incomplete change or module evidence selects all IDs."""
    def full(reason):
        return ScopeSelection("full", reason, frozenset(runnable_ids))

    if head_ref.startswith("land/batch-"):
        return full("landing-batch")
    repo = Path(repo).resolve()
    try:
        if git(repo, "rev-parse", "--verify", "HEAD^{commit}").strip() != head_sha:
            return full("git-error")
        if git(repo, "status", "--porcelain", "-z", "--untracked-files=all", "--", "src", "crates", "Cargo.toml"):
            return full("git-error")
        entries = git(repo, "diff", "--name-status", "--no-renames", "-z", f"{base_sha}...{head_sha}", "--")
        fields = entries.rstrip("\0").split("\0") if entries else []
        changes = dict(zip(fields[1::2], fields[::2], strict=True))
    except ValueError:
        return full("git-error")

    common = next((path for path in sorted(changes) if _shared_input(path)), None)
    if common:
        return full(f"shared-input:{common!r}")
    rust = sorted(path for path in changes if path.startswith("src/") and path.endswith(".rs"))
    if not rust:
        return full("no-rust-changes")
    unrecognized = next((path for path in rust if changes[path] not in {"A", "M"}), None)
    if unrecognized:
        return full(f"unmapped-module:{unrecognized!r}")

    try:
        names = select_modules(repo, head_sha, rust, {name for binary, name in runnable_ids if binary == "agentdesk"})
    except MappingError as exc:
        return full(str(exc))
    except GitQueryError:
        return full("git-error")
    except (OSError, UnicodeError):
        return full("unmapped-module")

    selected = {("agentdesk", name) for name in names}
    if not selected:
        return full("empty-scope")
    return ScopeSelection("scope", "changed-modules-and-core", frozenset((selected | set(core_ids)) & set(runnable_ids)))
