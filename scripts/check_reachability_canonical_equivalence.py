#!/usr/bin/env python3
"""Rust<->Python canonical obligation equivalence gate (#5071 T4-B2 = 4987 B1').

4987 §2.4 states the hazard: the obligation term "assistant text block" is
computed in dcserver (`health/reachability/obligation.rs`) AND in the
out-of-band watchdog (`relay_watchdog.py`), and two definitions are two oracles
of which one is always wrong. §-1.7 lists closing it as blocker **B1'**, and
§-1.5 widens the schema both must emit to
``(generation, start, end, identity, reason)``.

HOW THE EQUIVALENCE IS PROVED, AND WHAT THAT IS WORTH
-----------------------------------------------------
Through a golden corpus, not by running both runtimes in one process:

* this gate asserts Python's bytes equal ``tests/fixtures/relay_obligation/
  <case>.expected``;
* ``obligation::tests::canonical_output_matches_the_golden_corpus_byte_for_byte``
  asserts Rust's bytes equal the same files.

Equality with a common third value IS byte-equality between the two. The corpus
being a THIRD PARTY is the load-bearing part: a change applied identically to
both implementations still turns the corpus red, so "we drifted together" is not
a way through.

**This proves equality over the corpus, not over all inputs.** The two runtimes
do not share a JSON parser or a Unicode whitespace table. Known residual
differences, neither of which any corpus case can reach:

* ``str.strip()`` treats U+001C..U+001F as whitespace and Rust's ``str::trim``
  does not, so a text block consisting ONLY of those code points would classify
  as ``NO_ASSISTANT_TEXT`` in Python and ``ASSISTANT_TEXT`` in Rust;
* the timestamp rung runs ``time.strptime`` on one side and ``chrono`` on the
  other. The corpus pins the shapes that matter (ISO with millis and Z, exactly
  19 characters, single-digit fields, out-of-range month, short, empty,
  garbage); an exotic shape outside those is unproven.

Say what is measured, not what is hoped: adding a case is how the guarantee
grows.

WHAT THIS GATE RUNS
-------------------
1. **Corpus equivalence** — Python vs golden, every case, byte for byte, over a
   corpus asserted non-empty. An empty corpus must never read as a clean run.
2. **One-sided mutation** — each declared mutation edits exactly ONE
   implementation and must turn its side red. The Python mutations run
   in-process every time. The Rust mutations need a compiler and run under
   ``--with-rust``; in ordinary CI the Rust side is held by the corpus test in
   the ``test-non-pg`` lane, and this gate checks that the lane still selects
   it (below).
3. **Non-vacuous test selection** — every curated lane filter naming the
   reachability tree must match at least one row of
   ``scripts/lib_test_inventory_manifest.txt``. §4.1's third common gate: a
   filter that matches nothing lets ``cargo test`` exit 0 having run nothing.
4. **No judgment authority** — the source half of the T4-B2 invariant. The tree
   observes; nothing outside it may read it, and no bound may appear inside it.
   The DESTRUCTIVE half is not duplicated here: it is already enforced per file
   by ``scripts/check_destructive_call_site_ratchet.py``, whose four categories
   are exactly 4987's destructive surfaces, so a destructive call site appearing
   in this tree moves that ratchet.

Checks 3 and 4 are LINTS OVER SOURCE TEXT, not proofs — the same downgrade
``scripts/check_reachability_row_independence.py`` carries, for the same reason
(real enforcement needs a crate boundary). They reuse that gate's neutralizer,
which is `check_clippy_allow_ratchet.py`'s, so this repository keeps ONE Rust
lexical pre-pass rather than three.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

CORPUS_REL = Path("tests/fixtures/relay_obligation")
WATCHDOG_REL = Path("scripts/relay_watchdog.py")
OBLIGATION_REL = Path("src/services/discord/health/reachability/obligation.rs")
TREE_ROOT_FILE = "src/services/discord/health/reachability.rs"
TREE_DIR = "src/services/discord/health/reachability"
MANIFEST_REL = Path("scripts/lib_test_inventory_manifest.txt")
JUSTFILE_REL = Path("justfile")

# The corpus is the shared third party. Below this it stops covering the shapes
# 4987 §-1.5 names (CRLF, partial line, multi-byte, rotation) and the gate would
# be agreeing about too little to matter.
MIN_CORPUS_CASES = 10

RUST_CORPUS_TEST = (
    "services::discord::health::reachability::obligation::tests"
    "::canonical_output_matches_the_golden_corpus_byte_for_byte"
)

# Any other file naming the tree in CODE is a consumer this slice did not
# sanction: T4-B6 is where a reader of these verdicts lands, and it is gated
# behind `G-T4`. The one production entry point plus the module declaration. `framework_setup`
# is deliberately NOT here: it calls `run_bot_spawn_reachability_observation`,
# which lives in `spawns.rs`, and never names the module -- so the tree's entry
# point stays a single hop, and the check stays a check about the MODULE rather
# than about the substring.
ALLOWED_TREE_REFERENCES = {
    "src/services/discord/health.rs",
    "src/services/discord/runtime_bootstrap/spawns.rs",
}
# A module reference, not a word: `run_bot_spawn_reachability_observation` is an
# identifier that merely contains the substring, and matching it would make the
# allowlist a list of names rather than of readers.
TREE_REFERENCE_RE = re.compile(r"\breachability\s*::|\bmod\s+reachability\b")

# 4987 §10 lists hardcoding a threshold at S1 as NO-GO: the bounds are the
# OUTPUT of the 30-day observation this slice starts.
FORBIDDEN_BOUND_RE = re.compile(r"\b(warn_bound|fail_bound)\b")

LANE_FILTER_RE = re.compile(
    r"cargo test --lib (services::discord::health::reachability[A-Za-z0-9_:]*)"
)


def _load_neutralizer():
    """Reuse the repo's single Rust lexical pre-pass rather than forking it."""

    name = "check_clippy_allow_ratchet"
    if name in sys.modules:
        return sys.modules[name].neutralize_source
    spec = importlib.util.spec_from_file_location(
        name, Path(__file__).resolve().parent / "check_clippy_allow_ratchet.py"
    )
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load scripts/check_clippy_allow_ratchet.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module.neutralize_source


neutralize_source = _load_neutralizer()


@dataclass(frozen=True)
class Case:
    name: str
    generation_mtime_ns: int
    dev: int
    ino: int
    base_offset: int
    oversized_line_limit: int


def load_corpus(repo_root: Path) -> list[Case]:
    manifest = repo_root / CORPUS_REL / "cases.json"
    entries = json.loads(manifest.read_text(encoding="utf-8"))
    return [
        Case(
            name=entry["name"],
            generation_mtime_ns=entry["generation_mtime_ns"],
            dev=entry["dev"],
            ino=entry["ino"],
            base_offset=entry["base_offset"],
            oversized_line_limit=entry["oversized_line_limit"],
        )
        for entry in entries
    ]


def load_watchdog(repo_root: Path, source: str | None = None):
    """Load `relay_watchdog.py` under `repo_root`, optionally from mutated text.

    `repo_root` is a parameter and not the module constant so this gate's own
    mutation proof can point it at a synthetic tree; loading the real watchdog
    while claiming to check a synthetic one is how a gate test passes without
    testing the gate.

    Each load gets a FRESH module object under the same name, because
    `@dataclass` resolves its own module through `sys.modules` and fails on an
    unregistered one. Registering it is therefore not incidental: it is what
    lets a mutant be executed at all.
    """

    path = repo_root / WATCHDOG_REL
    spec = importlib.util.spec_from_file_location("relay_watchdog", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {WATCHDOG_REL}")
    module = importlib.util.module_from_spec(spec)
    sys.modules["relay_watchdog"] = module
    if source is None:
        spec.loader.exec_module(module)
    else:
        exec(compile(source, str(path), "exec"), module.__dict__)
    return module


def python_output(module, repo_root: Path, case: Case) -> bytes:
    data = (repo_root / CORPUS_REL / f"{case.name}.jsonl").read_bytes()
    records, next_offset = module.canonical_obligation_records(
        data,
        case.base_offset,
        case.generation_mtime_ns,
        case.dev,
        case.ino,
        case.oversized_line_limit,
    )
    return module.encode_canonical_records(records, next_offset)


def golden(repo_root: Path, case: Case) -> bytes:
    return (repo_root / CORPUS_REL / f"{case.name}.expected").read_bytes()


def check_corpus_equivalence(repo_root: Path, cases: list[Case]) -> list[str]:
    if len(cases) < MIN_CORPUS_CASES:
        return [
            f"the corpus declares {len(cases)} case(s), below the {MIN_CORPUS_CASES} "
            "floor; a thin corpus makes this gate agree about nothing"
        ]
    module = load_watchdog(repo_root)
    failures: list[str] = []
    for case in cases:
        actual = python_output(module, repo_root, case)
        expected = golden(repo_root, case)
        if actual != expected:
            failures.append(
                f"case {case.name}: python output != golden corpus\n"
                f"    --- python ---\n{actual.decode('utf-8', 'replace')}"
                f"    --- golden ---\n{expected.decode('utf-8', 'replace')}"
            )
    return failures


# ── One-sided mutations ───────────────────────────────────────────────────────
#
# Each entry edits exactly ONE implementation and must turn that side red. None
# is a compile-breaking edit: #5071 §4.1 gate 4 does not count a mutation killed
# by the compiler as behavioural evidence, so every mutation below is a
# semantic change to a rung, a boundary, or a cursor rule that still builds.

PYTHON_MUTATIONS: tuple[tuple[str, str, str], ...] = (
    (
        "crlf-not-stripped",
        'if content_end > line_start and data[content_end - 1 : content_end] == b"\\r":',
        'if content_end > line_start and data[content_end - 1 : content_end] == b"\\0":',
    ),
    (
        "harness-control-rung-removed",
        "    if is_harness_control_assistant_record(record):\n        return \"HARNESS_CONTROL\"",
        "    if False:\n        return \"HARNESS_CONTROL\"",
    ),
    (
        "timestamp-rung-removed",
        '    if parse_transcript_ts(record.get("timestamp", "")) is None:\n'
        '        return "UNPARSABLE_TIMESTAMP"',
        '    if False:\n        return "UNPARSABLE_TIMESTAMP"',
    ),
    (
        "blank-line-reason-respelled",
        '    if not line:\n        return "BLANK_LINE"',
        '    if not line:\n        return "MALFORMED_JSON"',
    ),
    (
        "partial-line-advances-the-cursor",
        '    records.append((generation_mtime_ns, base_offset + line_start,\n'
        '                    base_offset + len(data), dev, ino, "PARTIAL_LINE"))\n'
        "    return records, base_offset + line_start",
        '    records.append((generation_mtime_ns, base_offset + line_start,\n'
        '                    base_offset + len(data), dev, ino, "PARTIAL_LINE"))\n'
        "    return records, base_offset + len(data)",
    ),
    (
        "oversized-boundary-widened",
        "    if remainder >= oversized_line_limit:",
        "    if remainder > oversized_line_limit:",
    ),
    (
        "terminator-excluded-from-the-range",
        "            base_offset + terminator + 1,",
        "            base_offset + terminator,",
    ),
    (
        "identity-dropped-from-the-record",
        'f"{generation}\\t{start}\\t{end}\\t{dev}:{ino}\\t{reason}"',
        'f"{generation}\\t{start}\\t{end}\\t0:0\\t{reason}"',
    ),
)

RUST_MUTATIONS: tuple[tuple[str, str, str], ...] = (
    (
        "crlf-not-stripped",
        "if content_end > line_start && bytes[content_end - 1] == b'\\r' {",
        "if content_end > line_start && bytes[content_end - 1] == b'\\0' {",
    ),
    (
        "harness-control-rung-removed",
        "        return ObligationReason::HarnessControl;",
        "        return ObligationReason::NoAssistantText;",
    ),
    (
        "partial-line-advances-the-cursor",
        "        next_offset: base_offset + line_start as u64,",
        "        next_offset: base_offset + bytes.len() as u64,",
    ),
    (
        "oversized-boundary-widened",
        "    if remainder as u64 >= oversized_line_limit {",
        "    if remainder as u64 > oversized_line_limit {",
    ),
    (
        "blank-line-reason-respelled",
        'Self::BlankLine => "BLANK_LINE",',
        'Self::BlankLine => "MALFORMED_JSON",',
    ),
)


def run_python_mutations(repo_root: Path, cases: list[Case]) -> list[str]:
    source = (repo_root / WATCHDOG_REL).read_text(encoding="utf-8")
    survivors: list[str] = []
    for name, before, after in PYTHON_MUTATIONS:
        occurrences = source.count(before)
        if occurrences != 1:
            survivors.append(
                f"python mutation {name!r} anchors on text appearing {occurrences} "
                "time(s); a mutation that cannot be applied proves nothing"
            )
            continue
        module = load_watchdog(repo_root, source.replace(before, after))
        if any(
            python_output(module, repo_root, case) != golden(repo_root, case)
            for case in cases
        ):
            continue
        survivors.append(
            f"python mutation {name!r} SURVIVED: the corpus still matches with "
            "one implementation changed, so it does not pin this rule"
        )
    # Restore the unmutated module so a later caller in this process is not
    # left holding a mutant.
    load_watchdog(repo_root)
    return survivors


def run_rust_mutations(repo_root: Path) -> list[str]:
    target = repo_root / OBLIGATION_REL
    original = target.read_text(encoding="utf-8")
    survivors: list[str] = []
    try:
        baseline = subprocess.run(
            ["cargo", "test", "--lib", RUST_CORPUS_TEST, "--", "--exact"],
            cwd=repo_root,
            capture_output=True,
            text=True,
            check=False,
        )
        if baseline.returncode != 0:
            return [
                "the unmutated Rust corpus test is already red; a mutation runner "
                f"on a red baseline measures nothing.\n{baseline.stdout[-2000:]}"
            ]
        for name, before, after in RUST_MUTATIONS:
            occurrences = original.count(before)
            if occurrences != 1:
                survivors.append(
                    f"rust mutation {name!r} anchors on text appearing "
                    f"{occurrences} time(s); a mutation that cannot be applied "
                    "proves nothing"
                )
                continue
            target.write_text(original.replace(before, after), encoding="utf-8")
            result = subprocess.run(
                ["cargo", "test", "--lib", RUST_CORPUS_TEST, "--", "--exact"],
                cwd=repo_root,
                capture_output=True,
                text=True,
                check=False,
            )
            if result.returncode == 0:
                survivors.append(
                    f"rust mutation {name!r} SURVIVED: the corpus still matches "
                    "with one implementation changed"
                )
            elif "error[E" in result.stderr or "error: could not compile" in result.stderr:
                survivors.append(
                    f"rust mutation {name!r} broke the BUILD rather than the "
                    "assertion; #5071 §4.1 does not count a compile failure as "
                    "behavioural evidence"
                )
            print(f"  rust mutation {name}: killed" if result.returncode != 0 else "")
    finally:
        target.write_text(original, encoding="utf-8")
    return survivors


def check_lane_selection(repo_root: Path) -> list[str]:
    """Every reachability lane filter must select at least one real test."""

    justfile = (repo_root / JUSTFILE_REL).read_text(encoding="utf-8")
    filters = LANE_FILTER_RE.findall(justfile)
    if not filters:
        return [
            "no curated lane selects the reachability tree; its tests would run in "
            "no CI recipe at all"
        ]
    manifest = (repo_root / MANIFEST_REL).read_text(encoding="utf-8").splitlines()
    rows = [line for line in manifest if line and not line.startswith("#")]
    problems: list[str] = []
    for filt in sorted(set(filters)):
        matched = sum(1 for row in rows if filt in row)
        if matched == 0:
            problems.append(
                f"lane filter `{filt}` matches 0 rows of {MANIFEST_REL}; cargo would "
                "exit 0 having run nothing (#5071 §4.1 gate 3)"
            )
    return problems


def tree_files(repo_root: Path) -> list[Path]:
    files: list[Path] = []
    root_file = repo_root / TREE_ROOT_FILE
    if root_file.is_file():
        files.append(root_file)
    tree_dir = repo_root / TREE_DIR
    if tree_dir.is_dir():
        files.extend(sorted(p for p in tree_dir.rglob("*.rs") if p.is_file()))
    return files


def check_no_judgment_authority(repo_root: Path) -> list[str]:
    problems: list[str] = []

    owned = tree_files(repo_root)
    if not owned:
        return ["the reachability tree is absent; an empty scan is not a clean scan"]

    for path in owned:
        rel = path.relative_to(repo_root).as_posix()
        cleaned, ambiguous = neutralize_source(path.read_text(encoding="utf-8"))
        if ambiguous:
            problems.append(f"{rel}: unlexable source; failing closed")
            continue
        for match in FORBIDDEN_BOUND_RE.finditer(cleaned):
            line = cleaned.count("\n", 0, match.start()) + 1
            problems.append(
                f"{rel}:{line}: `{match.group(0)}` in code. 4987 §10 makes a "
                "hardcoded bound at S1 a NO-GO; the bounds are what the 30-day "
                "observation produces, and T4-B6 introduces them"
            )

    owned_set = {path.resolve() for path in owned}
    consumers: set[str] = set()
    for path in sorted((repo_root / "src").rglob("*.rs")):
        if path.resolve() in owned_set:
            continue
        rel = path.relative_to(repo_root).as_posix()
        cleaned, ambiguous = neutralize_source(path.read_text(encoding="utf-8"))
        if ambiguous:
            problems.append(f"{rel}: unlexable source; failing closed")
            continue
        if TREE_REFERENCE_RE.search(cleaned):
            consumers.add(rel)

    unexpected = sorted(consumers - ALLOWED_TREE_REFERENCES)
    if unexpected:
        problems.append(
            "the reachability tree grew a consumer: "
            + ", ".join(unexpected)
            + ". #5071 T4-B2 observes and records; a reader of its verdicts lands "
            "in T4-B6, behind `G-T4`"
        )
    missing = sorted(ALLOWED_TREE_REFERENCES - consumers)
    if missing:
        problems.append(
            "the expected wiring is gone: "
            + ", ".join(missing)
            + ". If the observation task was removed, remove its allowance here in "
            "the same change"
        )
    return problems


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--with-rust",
        action="store_true",
        help="also run the Rust half of the mutation runner (needs cargo; slow)",
    )
    parser.add_argument("--repo-root", default=str(REPO_ROOT))
    args = parser.parse_args(argv)
    repo_root = Path(args.repo_root).resolve()

    cases = load_corpus(repo_root)
    problems: list[str] = []
    problems += check_corpus_equivalence(repo_root, cases)
    problems += run_python_mutations(repo_root, cases)
    problems += check_lane_selection(repo_root)
    problems += check_no_judgment_authority(repo_root)
    if args.with_rust:
        problems += run_rust_mutations(repo_root)

    if problems:
        print("reachability canonical equivalence gate FAILED:", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1

    rust_note = (
        f"{len(RUST_MUTATIONS)} rust mutations killed"
        if args.with_rust
        else f"{len(RUST_MUTATIONS)} rust mutations declared (run --with-rust to "
        "execute; the Rust side is held in CI by the test-non-pg lane)"
    )
    print(
        f"reachability canonical equivalence OK: {len(cases)} corpus cases match "
        f"byte for byte, {len(PYTHON_MUTATIONS)} python mutations killed, "
        f"{rust_note}; lanes select real tests and the tree has no consumer "
        "beyond its own wiring (source lint, not a type proof)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
