"""Required-id exceptions read from the legacy file plus `.d` fragments, judged as one set."""

from __future__ import annotations

import datetime
import importlib.util
import re
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

from tests import test_lane_source_parity as harness

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "required_id_fragments_parity", ROOT / "scripts/lane_source_parity.py"
)
parity = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = parity
SPEC.loader.exec_module(parity)
registration = parity._sibling("lane_new_id_registration.py")
LEGACY = registration.EXCEPTIONS_REL.as_posix()
FRAGMENTS = registration.FRAGMENTS_REL.as_posix()
EARLY = datetime.date(2000, 1, 1)
ROW = "{} | {} | #5082 | expires={} | kept out of PR lanes on purpose"
HELPER = "{} | helper-module | #5082 | expires=2099-12-31 | shared fixtures for sibling tests"


def legacy_oracle(path: Path, today: datetime.date):
    """The pre-fragment legacy reader, frozen: legacy-only input must judge exactly as it did."""
    rows, findings, keys = [], [], []
    text = path.read_text("utf-8") if path.is_file() else ""
    for lineno, raw in enumerate(text.splitlines(), 1):
        row = raw.strip()
        if not row or row.startswith("#"):
            continue
        fields = [field.strip() for field in row.split("|")]
        location = f"{LEGACY}:{lineno}"
        expires = registration._EXPIRES.match(fields[3]) if len(fields) == 5 else None
        problem = None
        if len(fields) != 5 or not all(fields):
            problem = "expected `exact-test-id | mode | #issue | expires=YYYY-MM-DD | reason`"
        elif "*" in fields[0] or fields[0].endswith("::") or len(fields[0].split()) != 1:
            problem = "the key must be one exact test id, never a module prefix or pattern"
        elif fields[1] not in registration.MODE_KINDS:
            problem = f"mode must be one of {', '.join(sorted(registration.MODE_KINDS))}"
        elif not registration._ISSUE.match(fields[2]):
            problem = "issue must be a tracking `#<number>`"
        elif not expires:
            problem = "expiry must read `expires=YYYY-MM-DD`"
        elif len(fields[4].split()) < 3:
            problem = "the reason must say why in words, not a placeholder"
        keys.extend(fields[:1])
        if problem is None:
            try:
                date = datetime.date.fromisoformat(expires.group(1))
            except ValueError:
                problem = f"{expires.group(1)} is not a calendar date"
        if problem:
            findings.append(parity.Finding(registration.INVALID, location, problem))
        elif date < today:
            findings.append(parity.Finding(
                registration.STALE, location,
                f"`{fields[0]}` expired on {date}; register it or renew the row",
            ))
        else:
            rows.append((location, fields[0], fields[1]))
    if keys != sorted(set(keys)):
        findings.append(parity.Finding(
            registration.INVALID, LEGACY, "rows must be unique and sorted by test id"
        ))
    return rows, findings


class Layout(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / FRAGMENTS).mkdir(parents=True)
        (self.root / FRAGMENTS / "README.md").write_text("guide\n")

    def write(self, legacy: str | None, **fragments: list[str]) -> None:
        shutil.rmtree(self.root / FRAGMENTS)
        (self.root / FRAGMENTS).mkdir()
        if legacy is None:
            (self.root / LEGACY).unlink(missing_ok=True)
        else:
            (self.root / LEGACY).write_text(legacy, "utf-8")
        for name, rows in fragments.items():
            (self.root / FRAGMENTS / name).write_text("".join(f"{row}\n" for row in rows), "utf-8")

    def load(self, today: datetime.date = EARLY):
        rows, findings = registration.load_exceptions(parity, self.root / LEGACY, today)
        return [(row.location, row.test_id, row.mode) for row in rows], findings

    def details(self, findings) -> list[tuple[str, str, str]]:
        return [(f.kind, f.location, f.detail) for f in findings]


class LegacyCompatibilityTests(Layout):
    def test_legacy_only_input_judges_exactly_like_the_pre_fragment_reader(self) -> None:
        real = (ROOT / LEGACY).read_text("utf-8")
        corpus = {
            "real file": real,
            "empty": "",
            "comments only": "# header\n\n",
            "valid": "a::one | intentional-nonrequired | #1 | expires=2099-01-01 | three word reason\n",
            "invalid and expired, unsorted": (
                "# header\n"
                "z::late | platform-unavailable | #2 | expires=2001-01-01 | an expired row here\n"
                "b:: | model-unresolved | #3 | expires=2099-01-01 | a module prefix row\n"
                "c::x | unknown-mode | #4 | expires=2099-01-01 | a row with bad mode\n"
                "d::x | helper-module | 5 | expires=2099-01-01 | issue lacks hash mark\n"
                "e::x | helper-module | #6 | expires=2026-02-30 | not a calendar date\n"
                "f::x | helper-module | #7 | 2099-01-01 | expiry lacks its key\n"
                "g::x | helper-module | #8 | expires=2099-01-01 | short\n"
                "h::x | helper-module | #9\n"
                "| helper-module | #9 | expires=2099-01-01 | an empty key row\n"
            ),
        }
        for today in (EARLY, datetime.date(2026, 10, 14)):
            for name, text in corpus.items():
                with self.subTest(case=name, today=today):
                    self.write(text)
                    rows, findings = self.load(today)
                    self.assertEqual((rows, findings), legacy_oracle(self.root / LEGACY, today))
        self.write(None)
        self.assertEqual(self.load(), ([], []))

    def test_a_legacy_duplicate_stays_red_and_now_names_both_lines(self) -> None:
        row = ROW.format("a::one", "intentional-nonrequired", "2099-01-01")
        self.write(f"{row}\n{row}\n")
        rows, findings = self.load()
        self.assertEqual(findings[:1], legacy_oracle(self.root / LEGACY, EARLY)[1])
        self.assertEqual(self.details(findings[1:]), [
            (registration.INVALID, f"{LEGACY}:2", f"duplicate of {LEGACY}:1 for `a::one`")])
        self.assertEqual(rows, [(f"{LEGACY}:1", "a::one", "intentional-nonrequired")])

    def test_legacy_only_new_only_and_mixed_layouts_read_one_set(self) -> None:
        real = (ROOT / LEGACY).read_text("utf-8")
        legacy_rows, _ = legacy_oracle(ROOT / LEGACY, EARLY)
        lines = [line for line in real.splitlines() if line.strip() and not line.startswith("#")]
        self.assertEqual(len(lines), len(legacy_rows))
        expected = sorted((test_id, mode) for _, test_id, mode in legacy_rows)
        header = "".join(line + "\n" for line in real.splitlines() if line.startswith("#"))
        layouts = {
            "legacy-only": (real, {}),
            "new-only": (header, {"mac-mini-1-a.txt": lines[::2], "mac-book-2-b.txt": lines[1::2]}),
            "mixed": (header + "".join(line + "\n" for line in lines[::2]),
                      {"mac-mini-1-a.txt": lines[1::2]}),
        }
        for name, (legacy, fragments) in layouts.items():
            with self.subTest(layout=name):
                self.write(legacy, **fragments)
                rows, findings = self.load()
                self.assertEqual(findings, [])
                self.assertEqual(sorted((test_id, mode) for _, test_id, mode in rows), expected)
        rows, findings = registration.load_exceptions(parity, ROOT / LEGACY, EARLY)
        self.assertEqual(findings, [])
        self.assertTrue(set(expected) <= {(row.test_id, row.mode) for row in rows})


class FragmentRowTests(Layout):
    def test_fragment_rows_keep_their_location_and_every_five_field_check(self) -> None:
        self.write("", **{"mac-mini-1-p1.txt": [
            "# reviewed",
            ROW.format("a::one", "intentional-nonrequired", "2099-01-01"),
            ROW.format("b::two", "platform-unavailable", "2001-01-01"),
            "c::three | intentional-nonrequired | 5082 | expires=2099-01-01 | issue lacks hash",
        ]})
        rows, findings = self.load(datetime.date(2026, 10, 14))
        frag = f"{FRAGMENTS}/mac-mini-1-p1.txt"
        self.assertEqual(rows, [(f"{frag}:2", "a::one", "intentional-nonrequired")])
        self.assertEqual(self.details(findings), [
            (registration.STALE, f"{frag}:3",
             "`b::two` expired on 2001-01-01; register it or renew the row"),
            (registration.INVALID, f"{frag}:4", "issue must be a tracking `#<number>`"),
        ])

    def test_duplicates_conflicts_names_and_order_fail_with_both_locations(self) -> None:
        live = ROW.format("a::one", "intentional-nonrequired", "2099-01-01")
        other = ROW.format("a::one", "platform-unavailable", "2099-01-01")
        self.write(f"{live}\n", **{
            "mac-book-2-p1.txt": [live],
            "mac-mini-1-p1.txt": [ROW.format("z::last", "intentional-nonrequired", "2099-01-01"), other],
            "mac-mini-1-p2.text": [ROW.format("q::hidden", "intentional-nonrequired", "2001-01-01")],
        })
        rows, findings = self.load()
        book, mini = f"{FRAGMENTS}/mac-book-2-p1.txt", f"{FRAGMENTS}/mac-mini-1-p1.txt"
        got = self.details(findings)
        self.assertEqual(got[0][:2], (registration.INVALID, f"{FRAGMENTS}/mac-mini-1-p2.text"))
        self.assertIn((registration.INVALID, f"{mini}:2",
                       "rows must be unique and sorted by test id in a fragment"), got)
        self.assertIn((registration.INVALID, f"{book}:1", f"duplicate of {LEGACY}:1 for `a::one`"), got)
        conflict = [d for k, loc, d in got if loc == f"{mini}:2" and d.startswith("conflicts with")]
        self.assertEqual(len(conflict), 1)
        self.assertIn(f"conflicts with {LEGACY}:1 for `a::one`", conflict[0])
        self.assertEqual(sorted(test_id for _, test_id, _ in rows), ["a::one", "z::last"])


class FragmentRepo(harness.Repo):
    """The coverage CLI fixture, also writing `.d` fragment files for each state."""

    def __init__(self) -> None:
        super().__init__()
        self.id_fragments: dict[str, list[str]] = {}

    def write(self, root: Path) -> None:
        super().write(root)
        directory = root / FRAGMENTS
        shutil.rmtree(directory, ignore_errors=True)
        directory.mkdir()
        (directory / "README.md").write_text("guide\n", "utf-8")
        for name, rows in self.id_fragments.items():
            (directory / name).write_text("".join(f"{row}\n" for row in rows), "utf-8")


def debt_repo() -> FragmentRepo:
    repo = FragmentRepo()
    repo.modules["delta"] = ("old_case",)
    repo.baselines = (["delta"], ["delta"])
    return repo


def verdict(output: str) -> list[str]:
    """Pass/fail lines without the commit SHA and the fixture's absolute paths."""
    return [re.sub(r"base=[0-9a-f]{40}", "base=<ref>", line) for line in output.splitlines()
            if line.startswith(("OK: new_required_test_ids", "FAIL: new_required_test_ids"))]


class JudgedFragmentTests(unittest.TestCase):
    def test_a_fragment_row_excepts_exactly_like_the_same_legacy_row(self) -> None:
        row = ROW.format("delta::reads_pg_env", "intentional-nonrequired", "2099-12-31")

        def legacy(repo: FragmentRepo) -> None:
            harness.add_unselected(repo)
            repo.id_exceptions = [row]

        def fragment(repo: FragmentRepo) -> None:
            harness.add_unselected(repo)
            repo.id_fragments = {"mac-mini-1-p1.txt": [row]}

        legacy_result, legacy_output = debt_repo().delta(legacy)
        fragment_result, fragment_output = debt_repo().delta(fragment)
        self.assertEqual((legacy_result, fragment_result), (0, 0), legacy_output + fragment_output)
        self.assertIn("added=1 mapped=0 excepted=1", fragment_output)
        self.assertEqual(verdict(fragment_output), verdict(legacy_output))

    def test_expiry_mode_and_staleness_apply_to_fragment_rows(self) -> None:
        cases = {
            "deleted id": (harness.add_unselected, ["delta::gone", "delta::reads_pg_env"],
                           "intentional-nonrequired", "2099-12-31", "`delta::gone` is no lib inventory id"),
            "now required": (harness.add_required, ["delta::reads_pg_env"], "intentional-nonrequired",
                             "2099-12-31", "is now required through targeted"),
            "mode mismatch": (harness.add_unselected, ["delta::reads_pg_env"], "platform-unavailable",
                              "2099-12-31", "which mode platform-unavailable"),
            "expired": (harness.add_unselected, ["delta::reads_pg_env"], "intentional-nonrequired",
                        "2000-01-01", "expired on 2000-01-01"),
        }
        for name, (change, ids, mode, expires, detail) in cases.items():
            with self.subTest(case=name):
                def apply(r: FragmentRepo, change=change, ids=ids, mode=mode, expires=expires) -> None:
                    change(r)
                    r.id_fragments = {"mac-mini-1-p1.txt": [ROW.format(i, mode, expires) for i in ids]}

                result, output = debt_repo().delta(apply)
                stale = harness.new_findings(output, "stale-new-id-exception")
                self.assertEqual(len(stale), 1, output)
                self.assertIn(detail, stale[0])
                self.assertIn(f"{FRAGMENTS}/mac-mini-1-p1.txt:1", stale[0])
                self.assertEqual(result, 1)

    def test_helper_rows_in_fragments_except_empty_modules_and_go_stale(self) -> None:
        def excepted(repo: FragmentRepo) -> None:
            repo.lib_tail = "#[cfg(test)]\nmod spy {\n    pub fn helper() {}\n}\n"
            repo.id_fragments = {"mac-mini-1-p1.txt": [HELPER.format("spy")]}

        result, output = FragmentRepo().delta(excepted)
        self.assertEqual(harness.new_findings(output, "new-test-module-empty"), [], output)
        self.assertEqual(result, 0, output)
        result, output = FragmentRepo().delta(
            lambda r: setattr(r, "id_fragments", {"mac-mini-1-p1.txt": [HELPER.format("gone")]}))
        stale = harness.new_findings(output, "stale-new-id-exception")
        self.assertEqual(len(stale), 1, output)
        self.assertIn(f"{FRAGMENTS}/mac-mini-1-p1.txt:1: `gone` is no cfg(test) module", stale[0])

    def test_an_old_lane_legacy_row_merges_beside_main_fragments_and_a_clash_fails(self) -> None:
        def main_state() -> FragmentRepo:
            repo = debt_repo()
            repo.lib_tail = "#[cfg(test)]\nmod spy {\n    pub fn helper() {}\n}\n"
            repo.id_fragments = {"mac-mini-1-p1.txt": [HELPER.format("spy")]}
            return repo

        def old_lane(repo: FragmentRepo) -> None:
            harness.add_unselected(repo)
            repo.id_exceptions = [ROW.format("delta::reads_pg_env", "intentional-nonrequired", "2099-12-31")]

        result, output = main_state().delta(old_lane)
        self.assertEqual(result, 0, output)
        self.assertIn("added=1 mapped=0 excepted=1", output)

        def clashing_lane(repo: FragmentRepo) -> None:
            old_lane(repo)
            repo.id_exceptions.append(HELPER.format("spy").replace("#5082", "#6000"))

        result, output = main_state().delta(clashing_lane)
        invalid = harness.new_findings(output, "new-id-exception-invalid")
        self.assertTrue(any(f"{FRAGMENTS}/mac-mini-1-p1.txt:1: conflicts with {LEGACY}:2" in line
                            for line in invalid), output)
        self.assertEqual(result, 1)

    def test_a_misnamed_fragment_cannot_hide_a_row_or_pass_its_id(self) -> None:
        def misnamed(repo: FragmentRepo) -> None:
            harness.add_unselected(repo)
            repo.id_fragments = {"mac-mini-1-p1.text": [
                ROW.format("delta::reads_pg_env", "intentional-nonrequired", "2099-12-31")]}

        result, output = debt_repo().delta(misnamed)
        invalid = harness.new_findings(output, "new-id-exception-invalid")
        self.assertEqual(len(invalid), 1, output)
        self.assertIn(f"{FRAGMENTS}/mac-mini-1-p1.text: a fragment name must read", invalid[0])
        self.assertEqual(len(harness.new_findings(output, "new-id-unrequired")), 1, output)
        self.assertEqual(result, 1)


if __name__ == "__main__":
    unittest.main()
