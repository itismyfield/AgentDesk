#!/usr/bin/env python3
"""Every acquisition of a process-global `Mutex<()>` must recover the poison (#5185).

WHY THIS IS A SCRIPT AND NOT A COMMENT
--------------------------------------
The rule it enforces was written down in `src/config.rs` first, and the same
sweep that produced the rule then measured what a documentation-only rule is
worth. One real failure (`voice_pcm_harness_unattended_e2e`) reported itself as
eleven, because every later acquirer of `config::shared_test_env_lock()` spelled
its acquisition `.lock().unwrap()` and died on the `PoisonError` the first panic
left behind. That was repaired site by site. A later sweep then turned one real
failure (`session_resume::tests::resume_production_path_clears_stale_binding_and_rebinds_runtime`)
into 68 -- 67 of 73 panics were `PoisonError` -- because the repair had been
applied to *that one mutex* while `tui_prompt_dedupe::TEST_LOCK`, a different
process-global `Mutex<()>` serialising 66 tests, still propagated.

So the class recurred once after being fixed, and recurred again after being
documented. `grep` over the tree found no script, lint, or test asserting it.
This repository has a name for a gate that exists only as prose: #5003.

THE RULE
--------
For every process-global `std::sync::Mutex<()>` in `src/` -- a `static`, or a
function returning `&'static Mutex<()>` -- every `.lock()` on it must recover a
poisoned guard instead of propagating the error. `.unwrap()`, `.expect(..)` and
`?` propagate; `unwrap_or_else(|poison| poison.into_inner())` and an explicit
`Err(poisoned) => poisoned.into_inner()` match recover.

WHY IT STOPS AT `Mutex<()>`
---------------------------
The unit payload is what makes blanket recovery unconditionally safe: there is
no data invariant a panicking holder could have torn, so the poison flag carries
no information a waiter must respect. A `Mutex<HashMap<..>>` is different and is
deliberately out of scope -- recovering one of those is a decision about the
guarded value, not about lock hygiene. `tokio::sync::Mutex` is out of scope too:
it cannot be poisoned at all.

WHAT THIS DOES NOT CLOSE
------------------------
It is a text scan, not a borrow-checked analysis of receiver types.

* It flags the four spellings that were actually measured turning one failure
  into a cascade -- `.unwrap()`, `.expect(..)`, `?`, and an `unwrap_or_else`
  whose closure never calls `into_inner`. It does NOT demand that every other
  shape recover. An earlier revision did, and every one of its 32 findings was
  a false positive: a `match` that recovers in a different syntax, a forwarding
  shim (`turn_orchestrator::SharedEnvLock::lock`) that hands the `LockResult`
  to a caller which recovers, and `unwrap_or_else(PoisonError::into_inner)`
  written as a path rather than a closure. A gate that cries wolf 32 times is
  a gate that gets deleted, so this one asserts less and means it.
* A `.lock()` reached through an alias (`let m = &SOME_LOCK; m.lock()`) or
  through a helper that takes `&'static Mutex<()>` as a parameter is not
  attributed to the static, so it is neither checked nor reported. The
  inventory below names what IS attributed, and it is printed on every run so
  the covered set is reviewable rather than assumed.
* Conversely, a same-named local binding shadowing an inventory name would be
  checked as if it were the static. That direction is fail-closed: it can only
  demand recovery somewhere it was not required, never permit propagation.
* Declaring a new process-global `Mutex<()>` inside a macro body, or behind a
  type alias, hides it from the inventory.
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

# `static NAME: [LazyLock<|OnceLock<]Mutex<()>` in any std spelling. The type
# body is matched loosely up to the `Mutex<()>` so `std::sync::`, `sync::`, and
# a bare `Mutex` all land, while `Mutex<HashMap<..>>` cannot: `<()>` is exact.
STATIC_UNIT_MUTEX = re.compile(
    r"\bstatic\s+(?P<name>[A-Z_][A-Z0-9_]*)\s*:\s*"
    r"(?P<type>[^=;]*?\bMutex\s*<\s*\(\s*\)\s*>[^=;]*?)\s*=",
    re.DOTALL,
)

# `fn name(..) -> &'static ..Mutex<()>`: the accessor shape `shared_test_env_lock`
# uses, which hides its `static` inside the function body.
ACCESSOR_UNIT_MUTEX = re.compile(
    r"\bfn\s+(?P<name>[a-z_][a-z0-9_]*)\s*\([^)]*\)\s*->\s*"
    r"&\s*'static\s+(?P<type>[A-Za-z0-9_:]*)\bMutex\s*<\s*\(\s*\)\s*>",
)

# `tokio::sync::Mutex<()>` cannot be poisoned at all, so it is not in scope --
# and its `.lock().await` sites would otherwise be flagged wholesale.
TOKIO_MUTEX = re.compile(r"\btokio\s*::\s*sync\s*::")

# A `DashMap<K, Arc<Mutex<()>>>` of per-key mutexes contains the same characters
# as a process-global one and is NOT the same thing: those guards are scoped to
# a session or a channel, not to the process, so poison cannot cascade across
# unrelated tests. Only the singleton wrappers are in scope.
NOT_A_SINGLETON = re.compile(r"\b(?:Arc|Box|DashMap|HashMap|BTreeMap|Vec|RwLock)\b|,")

# How far after `.lock()` a recovery may legitimately sit. The longest recovering
# form in the tree is the `match`/`Err(poisoned) => poisoned.into_inner()` shape.
RECOVERY_WINDOW = 160

PROPAGATING = (
    (re.compile(r"^\s*\.\s*unwrap\s*\(\s*\)"), "unwrap()"),
    (re.compile(r"^\s*\.\s*expect\s*\("), "expect(..)"),
    (re.compile(r"^\s*\?"), "?"),
)

# `unwrap_or_else` is the recovering spelling only when the handler actually
# calls `into_inner`; `unwrap_or_else(|_| panic!(..))` is `unwrap()` in disguise.
FAKE_RECOVERY = re.compile(r"^\s*\.\s*unwrap_or_else\s*\(")

# Both the closure form `|poison| poison.into_inner()` and the path form
# `std::sync::PoisonError::into_inner` count as recovery.
RECOVERY = re.compile(r"\binto_inner\b")


@dataclass(frozen=True)
class Violation:
    path: str
    line: int
    lock: str
    form: str

    def render(self) -> str:
        return f"{self.path}:{self.line}: {self.lock}.lock().{self.form} propagates PoisonError"


def _strip_line_comments(text: str) -> str:
    """Blank out `//` comments so a documented `.lock().unwrap()` is not a site.

    Deliberately naive about `//` inside string literals: turning a string body
    into spaces can only remove candidate sites, and this file's own tests pin
    the shapes that matter.
    """
    out = []
    for line in text.splitlines(keepends=True):
        index = line.find("//")
        out.append(line if index < 0 else line[:index] + "\n")
    return "".join(out)


def discover_inventory(repo_root: Path, files: list[Path]) -> dict[str, list[str]]:
    """Map each process-global `Mutex<()>` name to the files declaring it."""
    inventory: dict[str, list[str]] = {}
    for path in files:
        text = _strip_line_comments(path.read_text("utf-8", errors="replace"))
        rel = str(path.relative_to(repo_root))
        for match in STATIC_UNIT_MUTEX.finditer(text):
            declared = match.group("type")
            if TOKIO_MUTEX.search(declared) or NOT_A_SINGLETON.search(declared):
                continue
            inventory.setdefault(match.group("name"), []).append(rel)
        for match in ACCESSOR_UNIT_MUTEX.finditer(text):
            if TOKIO_MUTEX.search(match.group("type")):
                continue
            inventory.setdefault(match.group("name"), []).append(rel)
    return {name: sorted(set(paths)) for name, paths in inventory.items()}


def scan_file(repo_root: Path, path: Path, names: frozenset[str]) -> list[Violation]:
    raw = path.read_text("utf-8", errors="replace")
    text = _strip_line_comments(raw)
    rel = str(path.relative_to(repo_root))
    violations: list[Violation] = []
    # `NAME.lock()` and `accessor().lock()`, tolerating the line breaks rustfmt
    # inserts between the receiver and the method.
    pattern = re.compile(
        r"\b(?P<name>" + "|".join(re.escape(name) for name in sorted(names)) + r")\b"
        r"\s*(?:\(\s*\))?\s*\.\s*lock\s*\(\s*\)"
    )
    for match in pattern.finditer(text):
        tail = text[match.end(): match.end() + RECOVERY_WINDOW]
        label = next(
            (label for propagator, label in PROPAGATING if propagator.match(tail)),
            None,
        )
        if label is None and FAKE_RECOVERY.match(tail) and not RECOVERY.search(tail):
            label = "unwrap_or_else(..) without into_inner"
        if label is not None:
            violations.append(
                Violation(rel, text.count("\n", 0, match.start()) + 1, match.group("name"), label)
            )
    return violations


def check(repo_root: Path, source_root: Path) -> int:
    files = sorted(source_root.rglob("*.rs"))
    if not files:
        print(f"FAIL: no Rust sources under {source_root}", file=sys.stderr)
        return 2
    inventory = discover_inventory(repo_root, files)
    if not inventory:
        print(
            f"FAIL: no process-global Mutex<()> found under {source_root}; the "
            "detector matched nothing, which is a broken scan rather than a "
            "clean tree",
            file=sys.stderr,
        )
        return 2
    names = frozenset(inventory)
    violations = [v for path in files for v in scan_file(repo_root, path, names)]
    statics = sum(len(paths) for paths in inventory.values())
    print(
        f"process-global Mutex<()> inventory: {len(inventory)} distinct name(s), "
        f"{statics} declaration site(s)"
    )
    for name in sorted(inventory):
        print(f"  {name}: {', '.join(inventory[name])}")
    if violations:
        print(
            f"FAIL: {len(violations)} acquisition(s) propagate PoisonError instead of "
            "recovering it:",
            file=sys.stderr,
        )
        for violation in sorted(violations, key=lambda v: (v.path, v.line)):
            print(f"  {violation.render()}", file=sys.stderr)
        print(
            "Spell the acquisition `.lock().unwrap_or_else(|poison| poison.into_inner())`. "
            "A process-global Mutex<()> guards mutual exclusion, not data, so a "
            "panicking holder leaves nothing torn -- propagating turns one real "
            "failure into a cascade of poison victims (measured: 1 -> 11, then 1 -> 68).",
            file=sys.stderr,
        )
        return 1
    print(f"test mutex poison recovery: {len(files)} file(s) scanned, 0 propagating acquisitions")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--source-root", type=Path, default=None)
    args = parser.parse_args(argv)
    repo_root = args.repo_root.resolve()
    source_root = (args.source_root or repo_root / "src").resolve()
    return check(repo_root, source_root)


if __name__ == "__main__":
    raise SystemExit(main())
