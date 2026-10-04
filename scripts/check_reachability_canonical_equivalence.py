#!/usr/bin/env python3
"""Judgment-authority source lint (not a type proof) and Rust corpus mutations.
The retired Python counterpart is not required by either remaining check."""

from __future__ import annotations

import argparse
import importlib.util
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

OBLIGATION_REL = Path("src/services/discord/health/reachability/obligation.rs")
TREE_ROOT_FILE = "src/services/discord/health/reachability.rs"
TREE_DIR = "src/services/discord/health/reachability"

RUST_CORPUS_TEST = (
    "services::discord::health::reachability::obligation::tests"
    "::canonical_output_matches_the_golden_corpus_byte_for_byte"
)

# Each allowlist grants a specific spelling, not blanket permission to consume
# the tree. The declaration file may only declare the module. The B2c spawn may
# directly call ledger/observation APIs; verdict consumption remains T4-B6.
DECLARATION_ONLY_TREE_REFERENCES = {
    "src/services/discord/health.rs",
}
OBSERVATION_ONLY_TREE_REFERENCES = {
    "src/services/discord/runtime_bootstrap/spawns.rs",
}
ALLOWED_TREE_REFERENCES = (
    DECLARATION_ONLY_TREE_REFERENCES | OBSERVATION_ONLY_TREE_REFERENCES
)
# Files sanctioned to READ the tree, each landed by a named slice. Unlike the
# allowances above, these may spell `reachability::` in code — but only as
# fully-qualified `reachability::divergence::` paths:
# `qualified_read_only_problems` reports any `use` item here naming the tree
# (an alias `use ... as rx;` and a re-export `pub use ...;` both republish it
# under a name this scan cannot recognise) and any occurrence outside that one
# sanctioned path, a `verdict` read above all. A qualified `type` alias
# republishing a tree type stays invisible to this lexical gate — the same
# lint-not-type-proof downgrade 4987 §-1.5 records for row independence.
#
# Since #5071 T4-B6 this tier is EMPTY: its only member, `health/snapshot.rs`
# (#5071 T4-B4, 4987 S4 — the descriptive row-coordinate divergence record),
# was promoted to `JUDGMENT_TREE_CONSUMERS` below, and the consumer loop in
# `check_no_judgment_authority` applies exactly one tier per file — a judgment
# allowance supersedes this sanction rather than stacking with it. The tier
# and `qualified_read_only_problems` stay: the next reader that needs only the
# descriptive `divergence` record joins here, not below.
SANCTIONED_TREE_CONSUMERS: set[str] = set()
# #5071 T4-B6 (4987 S3): the slice that lands judgment authority. These files
# may spell the composition, verdict, and external-verdict paths that every
# earlier slice was refused, and — unlike the T4-B4 sanction above — they may
# name those paths in a `use` item, because a tree type appearing as a struct
# FIELD has no fully-qualified spelling that survives `cargo fmt`.
#
# The two laundering shapes the T4-B4 rule was written against are still
# refused: an `as` rename and a `pub use` re-export both republish a tree item
# under a name `names_tree` cannot recognise, so a sibling could then read the
# tree while this gate reports no consumer. A plain private `use` launders
# nothing — the importing file is still counted as a consumer here, and the item
# is not visible past it.
#
# The r1 review found the two-STEP form of the same bypass, and
# `judgment_read_problems` now refuses it: a plain `use` here, then a `pub type
# Alias = ImportedTreeItem;` or a `pub use` of that imported name, and the
# sibling reads the tree without either file spelling `reachability` at the
# second step. The T4-B4 note above still holds for everything that is not a
# `use` or `type` item — a public fn signature, a public struct field, a trait
# associated type — where a tree type stays invisible to this lexical gate
# (4987 §-1.5's lint-not-type-proof downgrade). Those need a reviewer, not this
# scan.
#
#   * `health/snapshot.rs` — composes and publishes the verdict per channel and
#     applies the `RelayVerdictSource` polarity switch.
#   * `health/snapshot/relay_probe.rs` — the operand-assembly half of the file
#     above, and nothing else. #5942 r4 split it out because `health/snapshot.rs`
#     is a registered `shrink` giant (#5447) that r3 had grown past its pin; the
#     four items that moved (`RelayVerdictProbeOperands`,
#     `relay_verdict_probe_operands`, `reachability_ledger_operand_exists`,
#     `detail_executor_witness`) are the same code with the same callers, so
#     this is one reviewed file becoming two, not a new reader of the tree. It
#     is listed HERE rather than reading the ledger through its parent's private
#     `use`, which would have kept the allowance set unchanged at the cost of
#     making the file invisible to this scan — the outcome the tier exists to
#     prevent.
#   * `health/mailbox.rs` — carries the published report as a detail field.
#   * `health/stall_verdict.rs` — its detail-serialization test builds that field.
#   * `health/recovery.rs` — #5464 T5 S5's watchdog witness imports the ledger,
#     discovery identity, and verdict needed to build both axis-B fixtures.
#   * `relay_recovery.rs` — #5464 T5 S5's alias-invariant witness enumerates the
#     verdict variants consumed by the axis-B observation boundary.
#   * `relay_recovery/destructive_warrant.rs` — #5464 T5 S6a's sole runtime
#     consumer of the verdict operand; it applies the explicit 20-cell veto.
#   * `router/intake_gate/stale_turn.rs` — #5464 T5 S5's stale-turn witness
#     preserves a concrete verdict in the snapshot passed to axis-B observation.
JUDGMENT_TREE_CONSUMERS = {
    "src/services/discord/health/snapshot.rs",
    "src/services/discord/health/snapshot/relay_probe.rs",
    "src/services/discord/health/mailbox.rs",
    "src/services/discord/health/recovery.rs",
    "src/services/discord/health/stall_verdict.rs",
    "src/services/discord/relay_recovery.rs",
    "src/services/discord/relay_recovery/destructive_warrant.rs",
    "src/services/discord/router/intake_gate/stale_turn.rs",
}
# A module reference, not a word: an identifier that merely contains the
# substring (`run_bot_spawn_reachability_observation`, say) is not a reader, and
# matching it would make the allowlist a list of names rather than of readers.
TREE_REFERENCE_RE = re.compile(r"\breachability\s*::|\bmod\s+reachability\b")

# The bare module NAME. An ALIAS is a name too: `use super::reachability as rx;`
# followed by `rx::obligation::CANONICAL_SCHEMA_HEADER` consumes the tree without
# the substring `reachability::` ever appearing at the call site, and a scan that
# only knows qualified paths reports "no consumer" on a file that reads one. The
# same laundering works one hop further out — the allowlisted file re-exports a
# tree item under a new name and a sibling imports THAT — which is why the
# allowlist below grants the right to DECLARE the module, not to use it.
#
# Searched only where a false positive cannot come from an unrelated local
# called `reachability`: inside a `use` item, and inside the allowlisted files.
# `\b` already excludes `..._reachability_...` identifiers, since `_` is a word
# character.
TREE_NAME_RE = re.compile(r"\breachability\b")
MODULE_DECLARATION_RE = re.compile(r"\bmod\s+reachability\b")
OBSERVATION_REFERENCE_RE = re.compile(
    r"reachability\s*::\s*(?:ledger|observation)\s*::"
)
SANCTIONED_REFERENCE_RE = re.compile(r"reachability\s*::\s*divergence\s*::")
# The submodules #5071 T4-B6 unlocks, on top of `divergence`. `ledger` and
# `discovery` are absent on purpose: a judgment consumer reads the composed
# product through `composite`, and reaching for the ledger or the resolution
# ladder directly would rebuild the classification outside the one module that
# owns it.
JUDGMENT_REFERENCE_RE = re.compile(
    r"reachability\s*::\s*(?:composite|verdict|external_verdict|divergence)\s*::"
)
# `use ... as name;` renames a segment; `pub use ...` re-exports it. Both are the
# laundering the consumer scan cannot follow, so both stay refused even for a
# judgment consumer.
USE_RENAME_RE = re.compile(r"\bas\b")
PUB_USE_RE = re.compile(r"\bpub\b")
# `USE_ITEM_RE` starts at `use`, so a `pub use` item's visibility modifier falls
# OUTSIDE its match and a re-export reads as a plain import. This variant takes
# the optional `pub`/`pub(in ...)` prefix with it, which is what lets
# `judgment_read_problems` see the difference.
QUALIFIED_USE_ITEM_RE = re.compile(
    r"(?:\bpub\b\s*(?:\([^)]*\)\s*)?)?\buse\b[^;]*;", re.DOTALL
)
# The prefix `imported_item_names` strips before reading a `use` item's tree.
USE_ITEM_PREFIX_RE = re.compile(
    r"^\s*(?:\bpub\b\s*(?:\([^)]*\)\s*)?)?\buse\b\s*", re.DOTALL
)
# A `type` alias declared with ANY `pub` visibility — `pub`, `pub(crate)`,
# `pub(super)`, `pub(in path)`. A PRIVATE `type` alias is deliberately not
# matched: it binds a name inside one file and no sibling can import it, so it
# launders nothing. Group 2 is the right-hand side, which is where a tree item
# imported by a plain `use` would be republished under a fresh name.
PUBLISHED_TYPE_ALIAS_RE = re.compile(
    r"\bpub\b\s*(?:\([^)]*\)\s*)?\btype\b\s+(\w+)\b[^=;]*=\s*([^;]*);", re.DOTALL
)
IDENTIFIER_RE = re.compile(r"\b\w+\b")
# `check_reachability_row_independence.py` splits `use` items exactly this way,
# and for the same reason: the launderings a lexical scan CAN see are a bare
# trailing segment and an `as` rename, and both live inside a `use` item.
USE_ITEM_RE = re.compile(r"\buse\b[^;]*;", re.DOTALL)

# 4987 §10 lists hardcoding a threshold at S1 as NO-GO: the bounds are the
# OUTPUT of the 30-day observation this series starts.
#
# Case-insensitive and without word boundaries, so `OBLIGATION_WARN_BOUND_SECS`
# and `warnBound` are the same finding as `warn_bound`. The original `\b(...)\b`
# form let a rename walk straight past a gate whose whole point is the substance,
# and #5071 T4-B6 landing real bounds is exactly when that would have happened
# silently.
FORBIDDEN_BOUND_RE = re.compile(r"warn_?bound|fail_?bound", re.IGNORECASE)
# #5071 T4-B6 (4987 S3) is the slice §10 defers the bounds TO, and `composite` is
# the module that owns them. The scan still runs over every other file in the
# tree: an observation module growing its own threshold is the shape §10 rules
# out, and moving a bound out of `composite` would be a second judgment site.
BOUND_OWNER_RELS = {
    "src/services/discord/health/reachability/composite.rs",
    "src/services/discord/health/reachability/composite_tests.rs",
}


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


RUST_MUTATIONS: tuple[tuple[str, str, str], ...] = (
    (
        "crlf-not-stripped",
        "if content_end > line_start && bytes[content_end - 1] == b'\\r' {",
        "if content_end > line_start && bytes[content_end - 1] == b'\\0' {",
    ),
    (
        "harness-control-rung-removed",
        "        == Some(HARNESS_CONTROL_MODEL)\n"
        "    {\n"
        "        return ObligationReason::HarnessControl;\n"
        "    }\n"
        "    let timestamp = record",
        "        == Some(HARNESS_CONTROL_MODEL)\n"
        "    {\n"
        "        return ObligationReason::NoAssistantText;\n"
        "    }\n"
        "    let timestamp = record",
    ),
    (
        "partial-line-advances-the-cursor",
        "        // Deliberately NOT past the partial line: the next read frames it whole.\n"
        "        next_offset: base_offset + line_start as u64,\n"
        "    }\n"
        "}",
        "        // Deliberately NOT past the partial line: the next read frames it whole.\n"
        "        next_offset: base_offset + bytes.len() as u64,\n"
        "    }\n"
        "}",
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
    (
        # The Rust half of the same rung: a `text` that is not a string reads as
        # absent because `as_str()` yields None. Handing that None a non-blank
        # default makes an off-schema row an obligation here and nowhere else.
        # `.unwrap_or_default()` also ends the timestamp expression, so the
        # anchor carries the two lines above it to stay unique.
        "wrong-typed-text-defaulted-to-present",
        "                            .and_then(serde_json::Value::as_str)\n"
        "                            .unwrap_or_default()\n"
        "                            .trim()",
        "                            .and_then(serde_json::Value::as_str)\n"
        '                            .unwrap_or("?")\n'
        "                            .trim()",
    ),
)


def run_rust_mutations(repo_root: Path) -> list[str]:
    target = repo_root / OBLIGATION_REL
    original = target.read_text(encoding="utf-8")
    orphaned = [
        (name, original.count(after))
        for name, before, after in RUST_MUTATIONS
        if original.count(before) == 0 and original.count(after) > 0
    ]
    if orphaned:
        signatures = ", ".join(
            f"{name!r} (before anchor absent, after text appears {count} time(s))"
            for name, count in orphaned
        )
        return [
            f"target file {OBLIGATION_REL} contains residual declared rust "
            f"mutation signature(s): {signatures}. Inspect it with "
            f"`git diff -- {OBLIGATION_REL}`; restore only the mutated line for "
            "each detected signature named above to its declared before-anchor text"
        ]

    survivors: list[str] = []
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

        try:
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
            print(
                f"  rust mutation {name}: "
                + ("SURVIVED" if result.returncode == 0 else "killed")
            )
        finally:
            target.write_text(original, encoding="utf-8")
    return survivors


def tree_files(repo_root: Path) -> list[Path]:
    files: list[Path] = []
    root_file = repo_root / TREE_ROOT_FILE
    if root_file.is_file():
        files.append(root_file)
    tree_dir = repo_root / TREE_DIR
    if tree_dir.is_dir():
        files.extend(sorted(p for p in tree_dir.rglob("*.rs") if p.is_file()))
    return files


def names_tree(cleaned: str) -> bool:
    """Whether this file's CODE names the reachability module, alias included.

    Three spellings, because all three make the file a reader: a qualified path,
    the module declaration, and a `use` item carrying the bare segment — which
    covers `use super::reachability as rx;` and `use super::{reachability};`,
    the two shapes that consume the tree without writing `reachability::`.
    """

    if TREE_REFERENCE_RE.search(cleaned):
        return True
    return any(
        TREE_NAME_RE.search(item.group(0)) for item in USE_ITEM_RE.finditer(cleaned)
    )


def declaration_only_problems(rel: str, cleaned: str) -> list[str]:
    """Hold an allowlisted file to declaring the module and nothing more.

    The allowlist exists so the `mod` item that brings the tree into the crate
    is not itself reported as a consumer. It is NOT a licence to use the tree:
    a `pub use reachability::obligation::X as Y;`, a `type Y = reachability::
    ...;`, or a function returning a tree type republishes it under a name the
    consumer scan cannot recognise, and every other file in `src/` can then read
    the tree while this gate prints "no consumer". The contract T4-B2a signs is
    "machine-checked zero consumers", so the laundering has to die at the source
    of the alias rather than at each of its uses.
    """

    problems: list[str] = []
    declarations = list(MODULE_DECLARATION_RE.finditer(cleaned))
    for match in TREE_NAME_RE.finditer(cleaned):
        if any(
            declaration.start() <= match.start() < declaration.end()
            for declaration in declarations
        ):
            continue
        line = cleaned.count("\n", 0, match.start()) + 1
        problems.append(
            f"{rel}:{line}: this file is allowlisted to DECLARE `mod "
            "reachability`, not to use it. Naming the tree anywhere else here "
            "re-exports it under a name the consumer scan cannot see, and "
            "#5071 T4-B2a's inactivity is then unenforced. The observation "
            "task lands its own reader in T4-B2c; widen this gate there, in "
            "the same change"
        )
    return problems


def observation_only_problems(rel: str, cleaned: str) -> list[str]:
    """Permit direct B2c recording calls while rejecting judgment access.

    Every occurrence of the module name must begin a qualified path into the
    ledger or observation module. This deliberately rejects aliases and
    re-exports: either could hide a later verdict read from this lexical gate.
    """

    problems: list[str] = []
    for match in TREE_NAME_RE.finditer(cleaned):
        if OBSERVATION_REFERENCE_RE.match(cleaned, match.start()) is not None:
            continue
        line = cleaned.count("\n", 0, match.start()) + 1
        problems.append(
            f"{rel}:{line}: the T4-B2c wiring may name only direct "
            "`reachability::ledger::` and `reachability::observation::` paths. "
            "Aliases, re-exports, and verdict reads can hide judgment authority; "
            "verdict consumption remains T4-B6 behind `G-T4`"
        )
    return problems


def qualified_read_only_problems(rel: str, cleaned: str) -> list[str]:
    """Hold a sanctioned consumer to fully-qualified `divergence` reads.

    A sanctioned consumer may call into the tree, so its laundering rule is
    narrower than declaration-only, and it carries two obligations. First, no
    `use` item may name the tree: an alias (`use super::reachability as rx;`)
    and a re-export (`pub use super::reachability::...;`) both live in `use`
    items and both republish the tree under a name the consumer scan cannot
    recognise; a fully-qualified path at each read site is the one spelling
    this scan keeps seeing. Second, every remaining occurrence must begin a
    `reachability::divergence::` path — the sanction names that one read, so a
    fully-qualified `verdict` read is rejected here rather than surviving as a
    spelling this gate happens not to look at.
    """

    problems: list[str] = []
    use_spans = [
        (item.start(), item.end())
        for item in USE_ITEM_RE.finditer(cleaned)
        if TREE_NAME_RE.search(item.group(0)) is not None
    ]
    for match in TREE_NAME_RE.finditer(cleaned):
        line = cleaned.count("\n", 0, match.start()) + 1
        if any(start <= match.start() < end for start, end in use_spans):
            problems.append(
                f"{rel}:{line}: a sanctioned consumer must read the tree "
                "through fully-qualified paths only. A `use` item naming it is "
                "an alias or re-export this gate cannot track past; spell the "
                "path at the call site"
            )
            continue
        if SANCTIONED_REFERENCE_RE.match(cleaned, match.start()) is None:
            problems.append(
                f"{rel}:{line}: this sanction covers exactly the descriptive "
                "`reachability::divergence::` read (#5071 T4-B4). Any other "
                "tree path — a verdict read above all — is judgment authority "
                "no slice before T4-B6 holds, behind `G-T4`"
            )
    return problems


def split_top_level_use_parts(body: str) -> list[str]:
    """Split a `use` item's body on commas that are not inside a brace group."""

    parts: list[str] = []
    depth = 0
    current: list[str] = []
    for char in body:
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
        if char == "," and depth == 0:
            parts.append("".join(current))
            current = []
        else:
            current.append(char)
    parts.append("".join(current))
    return [part.strip() for part in parts if part.strip()]


def use_body_names(body: str) -> set[str]:
    """The names a `use` item body binds into the importing file's namespace.

    The trailing segment of each leaf, with nested brace groups expanded, so
    `use a::{b::{C}, D as E};` binds `C` and `E`. A glob leaf yields NOTHING
    here — the names it binds are not in the text — so this returns what the
    item names explicitly and no more. Callers must not read an empty result as
    "binds nothing".
    """

    names: set[str] = set()
    for part in split_top_level_use_parts(body):
        if part.endswith("}") and "{" in part:
            names |= use_body_names(part[part.index("{") + 1 : -1])
            continue
        renamed = re.split(r"\bas\b", part, maxsplit=1)
        leaf = renamed[-1].strip() if len(renamed) > 1 else part.rsplit("::", 1)[-1]
        leaf = leaf.strip()
        if leaf and leaf not in {"self", "*", "_"} and IDENTIFIER_RE.fullmatch(leaf):
            names.add(leaf)
    return names


def imported_item_names(use_item: str) -> set[str]:
    """The names one whole `use` item — visibility prefix included — binds."""

    body = USE_ITEM_PREFIX_RE.sub("", use_item.strip(), count=1).rstrip().rstrip(";")
    return use_body_names(body)


def use_body_source_names(body: str) -> set[str]:
    """The names a `use` item body READS, before any `as` rebinding.

    `use_body_names` answers "what does this item bind"; a republication check
    also has to answer "what does it take" — `pub use self::X as Y;` binds `Y`
    but takes `X`, and it is `X` that the file imported from the tree. Same
    brace expansion, same glob caveat: a glob spells no source name either.
    """

    names: set[str] = set()
    for part in split_top_level_use_parts(body):
        if part.endswith("}") and "{" in part:
            names |= use_body_source_names(part[part.index("{") + 1 : -1])
            continue
        source = re.split(r"\bas\b", part, maxsplit=1)[0]
        leaf = source.rsplit("::", 1)[-1].strip()
        if leaf and leaf not in {"self", "*", "_"} and IDENTIFIER_RE.fullmatch(leaf):
            names.add(leaf)
    return names


def imported_item_source_names(use_item: str) -> set[str]:
    """The names one whole `use` item reads, before any `as` rebinding."""

    body = USE_ITEM_PREFIX_RE.sub("", use_item.strip(), count=1).rstrip().rstrip(";")
    return use_body_source_names(body)


def judgment_read_problems(rel: str, cleaned: str) -> list[str]:
    """Hold a #5071 T4-B6 judgment consumer to unlaundered tree reads.

    Three obligations, narrower than `qualified_read_only_problems` on the `use`
    axis and wider on the path axis. First, a `use` item naming the tree may not
    rename (`as`) or re-export (`pub use`) it: those are the two spellings that
    put a tree item behind a name `names_tree` cannot see, which is what would
    let a sibling read the tree while this gate reports no consumer. Second,
    every MODULE REFERENCE outside a `use` item must begin one of the paths
    T4-B6 unlocks; `ledger::` and `discovery::` stay refused here, so a consumer
    cannot rebuild the classification outside `composite`.

    Third — the two-step form the first rule alone missed. The names a plain
    `use` binds are collected, and this file may not then PUBLISH one of them
    under a fresh spelling: not as a `pub` `type` alias whose right-hand side
    names one, and not as a `pub use` of one. Both hand a tree item to a sibling
    that never writes `reachability`, so the sibling is not a consumer by
    `names_tree` and this gate reports a clean tree while the item is being read
    outside every allowance. A PRIVATE `type` alias is left alone: it binds a
    name inside this file only, which is a spelling, not a republication.

    Outside a `use` item this scans for module references (`reachability::`,
    `mod reachability`), not for the bare word: 4987 §4.4 names the published
    detail object `reachability`, so these files legitimately carry a field, a
    binding, and a parameter with that name, and none of them reads anything.
    Inside a `use` item the bare-name scan still applies, because that is where
    a rename can hide.
    """

    problems: list[str] = []
    use_spans = [
        (item.start(), item.end(), item.group(0))
        for item in QUALIFIED_USE_ITEM_RE.finditer(cleaned)
        if TREE_NAME_RE.search(item.group(0)) is not None
    ]
    imported_from_tree: set[str] = set()
    for _start, _end, text in use_spans:
        imported_from_tree |= imported_item_names(text)
    for start, _end, text in use_spans:
        line = cleaned.count("\n", 0, start) + 1
        if USE_RENAME_RE.search(text) is not None:
            problems.append(
                f"{rel}:{line}: a judgment consumer may import the tree but may "
                "not rename it — an `as` alias republishes a tree item under a "
                "name this gate cannot follow"
            )
        if PUB_USE_RE.search(text) is not None:
            problems.append(
                f"{rel}:{line}: a judgment consumer may import the tree but may "
                "not re-export it — a `pub use` hands the tree to files that "
                "hold no allowance here"
            )
    for alias in PUBLISHED_TYPE_ALIAS_RE.finditer(cleaned):
        republished = sorted(
            imported_from_tree.intersection(IDENTIFIER_RE.findall(alias.group(2)))
        )
        if not republished:
            continue
        line = cleaned.count("\n", 0, alias.start()) + 1
        problems.append(
            f"{rel}:{line}: `pub type {alias.group(1)}` republishes "
            + ", ".join(f"`{name}`" for name in republished)
            + ", which this file imported from the tree. A judgment consumer may "
            "import a tree item; publishing it under a second name hands it to "
            "siblings that never spell `reachability`, so they read the tree "
            "while this gate counts them as bystanders. Keep the alias private "
            "or give the sibling its own reviewed allowance"
        )
    for item in QUALIFIED_USE_ITEM_RE.finditer(cleaned):
        if any(start <= item.start() < end for start, end, _ in use_spans):
            continue  # already judged by the rename/re-export rules above
        if PUB_USE_RE.search(item.group(0)) is None:
            continue
        republished = sorted(
            imported_from_tree.intersection(
                imported_item_names(item.group(0))
                | imported_item_source_names(item.group(0))
            )
        )
        if not republished:
            continue
        line = cleaned.count("\n", 0, item.start()) + 1
        problems.append(
            f"{rel}:{line}: this `pub use` re-exports "
            + ", ".join(f"`{name}`" for name in republished)
            + ", which this file imported from the tree. The item leaves under a "
            "path that never names `reachability`, which is the same laundering "
            "a `pub use` of the tree path is refused for"
        )
    for match in TREE_REFERENCE_RE.finditer(cleaned):
        if any(start <= match.start() < end for start, end, _ in use_spans):
            continue
        if JUDGMENT_REFERENCE_RE.match(cleaned, match.start()) is None:
            line = cleaned.count("\n", 0, match.start()) + 1
            problems.append(
                f"{rel}:{line}: #5071 T4-B6 unlocks the `composite`, `verdict`, "
                "`external_verdict` and `divergence` paths for this file. Any "
                "other tree path — the ledger and the resolution ladder above "
                "all — belongs to the composition module, not to its consumers"
            )
    return problems


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
        if rel not in BOUND_OWNER_RELS:
            for match in FORBIDDEN_BOUND_RE.finditer(cleaned):
                line = cleaned.count("\n", 0, match.start()) + 1
                problems.append(
                    f"{rel}:{line}: `{match.group(0)}` in code. 4987 §10 makes a "
                    "hardcoded bound outside the composition a NO-GO; the bounds "
                    "are what the 30-day observation produces, and T4-B6 gave "
                    "them to `composite` alone"
                )
    missing_bound_owners = sorted(
        BOUND_OWNER_RELS
        - {path.relative_to(repo_root).as_posix() for path in owned}
    )
    if missing_bound_owners:
        problems.append(
            "the bound owner is gone: "
            + ", ".join(missing_bound_owners)
            + ". If the composition module moved or was removed, move its "
            "`BOUND_OWNER_RELS` entry here in the same change instead of "
            "leaving an allowance that covers nothing"
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
        if not names_tree(cleaned):
            continue
        consumers.add(rel)
        if rel in DECLARATION_ONLY_TREE_REFERENCES:
            problems += declaration_only_problems(rel, cleaned)
        if rel in OBSERVATION_ONLY_TREE_REFERENCES:
            problems += observation_only_problems(rel, cleaned)
        if rel in JUDGMENT_TREE_CONSUMERS:
            problems += judgment_read_problems(rel, cleaned)
        elif rel in SANCTIONED_TREE_CONSUMERS:
            problems += qualified_read_only_problems(rel, cleaned)

    every_allowance = (
        ALLOWED_TREE_REFERENCES | SANCTIONED_TREE_CONSUMERS | JUDGMENT_TREE_CONSUMERS
    )
    unexpected = sorted(consumers - every_allowance)
    if unexpected:
        problems.append(
            "the reachability tree grew a consumer: "
            + ", ".join(unexpected)
            + ". Every reader joins its own allowance deliberately, in its own "
            "reviewed slice: T4-B2c wired the observation task, T4-B4 "
            "sanctioned the descriptive divergence record, and T4-B6 opened "
            "the composed verdict to the four files named here"
        )
    missing = sorted(every_allowance - consumers)
    if missing:
        problems.append(
            "the expected wiring is gone: "
            + ", ".join(missing)
            + ". If an expected declaration, observation consumer, "
            "sanctioned read, or judgment consumer was removed, "
            "remove its allowance here in the same change"
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

    problems: list[str] = []
    problems += check_no_judgment_authority(repo_root)
    if args.with_rust:
        problems += run_rust_mutations(repo_root)

    if problems:
        print("reachability judgment-authority gate FAILED:", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1

    rust_note = (
        f"{len(RUST_MUTATIONS)} rust mutations killed"
        if args.with_rust
        else f"{len(RUST_MUTATIONS)} rust mutations declared (run --with-rust to "
        "execute; the Rust side is held in CI by the test-non-pg obligation lane)"
    )
    print(
        "reachability judgment-authority OK: "
        f"{rust_note}; the tree has exactly its declaration, its B2c "
        f"observation consumer, {len(SANCTIONED_TREE_CONSUMERS)} sanctioned "
        f"qualified-path reader(s) and {len(JUDGMENT_TREE_CONSUMERS)} T4-B6 "
        "judgment consumer(s), with ledger/discovery reads rejected outside the "
        "tree and no allowed file renaming, re-exporting or publicly "
        "type-aliasing what it imported from the tree (source lint, not a type "
        "proof: a public fn signature, struct field or trait item carrying a "
        "tree type still passes this scan)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
