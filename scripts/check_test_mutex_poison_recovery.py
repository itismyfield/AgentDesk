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
`Err(poisoned) => poisoned.into_inner()` match recover. A direct
`if let Ok(..) = LOCK.lock()` or `while let Ok(..) = LOCK.lock()` discards the
poison and is rejected too.

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
* A direct `if let Ok(..) = NAME.lock()` or `while let Ok(..) = NAME.lock()`
  is classified from the lock result's consumer and rejected. The classifier
  tokenizes only the surrounding identifiers and delimiters; it is deliberately
  not a regular expression for Rust grammar. Once the mutex is poisoned the
  `Ok` arm never matches again, so the critical section is silently SKIPPED
  and mutual exclusion is lost for every later acquirer -- with no panic, no
  `PoisonError` in the transcript, and nothing for a cascade to point at.
  Parenthesized or block-wrapped lock expressions, `let ... else`, and other
  consumer forms are not inferred by this narrow check.
* An acquisition that DISCARDS poison through `.lock().ok()` (or a longer
  `.ok()` chain) remains unflagged. That is a known hole, not a claim that the
  shape is safe; the fixture tests pin this non-guarantee so a future change in
  the scanner cannot silently widen the contract.
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
class RustToken:
    text: str
    start: int
    end: int


def _is_identifier(text: str) -> bool:
    return bool(text) and (text[0].isalpha() or text[0] == "_") and all(
        char.isalnum() or char == "_" for char in text[1:]
    )


def _skip_quoted(text: str, start: int, quote: str) -> int:
    """Return the first position after a quoted Rust string/character."""
    index = start + 1
    while index < len(text):
        if text[index] == "\\":
            index += 2
        elif text[index] == quote:
            return index + 1
        else:
            index += 1
    return len(text)


def _skip_block_comment(text: str, start: int) -> int:
    """Skip a possibly nested Rust block comment."""
    depth = 1
    index = start + 2
    while index < len(text) and depth:
        if text.startswith("/*", index):
            depth += 1
            index += 2
        elif text.startswith("*/", index):
            depth -= 1
            index += 2
        else:
            index += 1
    return index


def _lex_rust_tokens(text: str) -> list[RustToken]:
    """Tokenize enough Rust to classify a direct lock-result consumer.

    This is intentionally a lexer, not a grammar regex. It preserves token
    offsets for the existing lock-site matcher and leaves full Rust parsing to
    rustc; the consumer check only needs identifiers, delimiters, and `=`.
    """
    tokens: list[RustToken] = []
    index = 0
    multi_char = (
        "::",
        "=>",
        "==",
        "!=",
        "<=",
        ">=",
        "&&",
        "||",
        "->",
        "+=",
        "-=",
        "*=",
        "/=",
        "%=",
        "&=",
        "|=",
        "^=",
        "<<",
        ">>",
    )
    while index < len(text):
        char = text[index]
        if char.isspace():
            index += 1
            continue
        if text.startswith("/*", index):
            index = _skip_block_comment(text, index)
            continue
        if char == '"':
            index = _skip_quoted(text, index, '"')
            continue
        # A lifetime (`'static`) is tokenized, while a character literal is
        # skipped so its contents cannot look like a consumer prefix.
        if char == "'":
            if index + 2 < len(text) and text[index + 2] == "'":
                index = _skip_quoted(text, index, "'")
                continue
            tokens.append(RustToken(char, index, index + 1))
            index += 1
            continue
        if char.isalpha() or char == "_":
            end = index + 1
            while end < len(text) and (text[end].isalnum() or text[end] == "_"):
                end += 1
            tokens.append(RustToken(text[index:end], index, end))
            index = end
            continue
        match = next((operator for operator in multi_char if text.startswith(operator, index)), None)
        if match is not None:
            tokens.append(RustToken(match, index, index + len(match)))
            index += len(match)
            continue
        tokens.append(RustToken(char, index, index + 1))
        index += 1
    return tokens


def _delimiter_pairs(tokens: list[RustToken]) -> dict[int, int]:
    """Map matching `()`, `[]`, and `{}` token indexes in valid-ish Rust."""
    opening = {"(": ")", "[": "]", "{": "}"}
    closing = {value: key for key, value in opening.items()}
    stack: list[tuple[str, int]] = []
    pairs: dict[int, int] = {}
    for index, token in enumerate(tokens):
        if token.text in opening:
            stack.append((token.text, index))
        elif token.text in closing:
            expected = closing[token.text]
            for stack_index in range(len(stack) - 1, -1, -1):
                if stack[stack_index][0] == expected:
                    _, opening_index = stack[stack_index]
                    del stack[stack_index:]
                    pairs[index] = opening_index
                    pairs[opening_index] = index
                    break
    return pairs


def _discarding_consumer(
    tokens: list[RustToken],
    pairs: dict[int, int],
    token_by_start: dict[int, int],
    lock_match: re.Match[str],
) -> str | None:
    """Classify a direct `if|while let Ok(..) = <lock>` consumer.

    The receiver may have Rust path qualifiers (`crate::module::LOCK`), but
    wrappers and aliases are deliberately outside this bounded contract.
    """
    receiver = token_by_start.get(lock_match.start())
    if receiver is None or tokens[receiver].text != lock_match.group("name"):
        return None
    while (
        receiver >= 2
        and tokens[receiver - 1].text == "::"
        and _is_identifier(tokens[receiver - 2].text)
    ):
        receiver -= 2
    if receiver == 0 or tokens[receiver - 1].text != "=":
        return None
    close = receiver - 2
    if close < 0 or tokens[close].text != ")":
        return None
    opening = pairs.get(close)
    if opening is None or opening < 3:
        return None
    ok = opening - 1
    let = ok - 1
    kind = let - 1
    if (
        tokens[ok].text != "Ok"
        or tokens[let].text != "let"
        or tokens[kind].text not in {"if", "while"}
    ):
        return None
    return f"{tokens[kind].text} let Ok(..)"


@dataclass(frozen=True)
class Violation:
    path: str
    line: int
    lock: str
    form: str

    def render(self) -> str:
        if self.form in {"if let Ok(..)", "while let Ok(..)"}:
            return (
                f"{self.path}:{self.line}: {self.lock}.lock().{self.form} "
                "discards PoisonError and silently skips the critical section"
            )
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
    matches = list(pattern.finditer(text))
    if not matches:
        return violations
    tokens = _lex_rust_tokens(text)
    pairs = _delimiter_pairs(tokens)
    token_by_start = {token.start: index for index, token in enumerate(tokens)}
    for match in matches:
        tail = text[match.end(): match.end() + RECOVERY_WINDOW]
        label = next(
            (label for propagator, label in PROPAGATING if propagator.match(tail)),
            None,
        )
        if label is None and FAKE_RECOVERY.match(tail) and not RECOVERY.search(tail):
            label = "unwrap_or_else(..) without into_inner"
        if label is None:
            label = _discarding_consumer(tokens, pairs, token_by_start, match)
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
            f"FAIL: {len(violations)} acquisition(s) propagate or discard PoisonError "
            "instead of recovering it:",
            file=sys.stderr,
        )
        for violation in sorted(violations, key=lambda v: (v.path, v.line)):
            print(f"  {violation.render()}", file=sys.stderr)
        print(
            "Spell the acquisition `.lock().unwrap_or_else(|poison| poison.into_inner())`. "
            "Do not consume its result with `if let Ok(..)` or `while let Ok(..)`: "
            "a process-global Mutex<()> guards mutual exclusion, not data, so a "
            "panicking holder leaves nothing torn -- propagating or silently "
            "skipping turns one real failure into a cascade (measured: 1 -> 11, "
            "then 1 -> 68).",
            file=sys.stderr,
        )
        return 1
    print(
        f"test mutex poison recovery: {len(files)} file(s) scanned, "
        "0 propagating or discarding acquisitions"
    )
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
