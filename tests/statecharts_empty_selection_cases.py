"""Empty-selection fixtures shared by the TLC check and aggregation CLI tests."""

import json
from contextlib import nullcontext
from unittest import mock


def empty_reports(test, count, name="reports"):
    reports = test.repo / name
    reports.mkdir()
    for index in range(count):
        (reports / f"tlc-{index}.json").write_text(json.dumps({
            "shard_index": index, "shard_count": count, "run_attempt": 1, "selected": [],
            "started": 0, "seconds": 0, "results": [], "problems": [],
        }))
    return reports


def assert_documentation_coverage_skip(test) -> None:
    reports = empty_reports(test, 4)
    rc, out, err, checked = test.run_main("--aggregate-reports", str(reports), "--shard-count", "4")
    test.assertEqual((rc, err), (0, ""))
    test.assertIn("NO_TARGETS(skip)", out)
    test.assertNotIn("PASS", out)
    checked.assert_not_called()


def assert_documentation_skip(test) -> None:
    reports = test.repo / "reports"
    reports.mkdir()
    summary = test.repo / "summary.md"
    for index in range(4):
        path = reports / f"tlc-{index}.json"
        rc, out, err, checked = test.run_main("--shard-count", "4", "--shard-index", str(index),
                                              "--report-json", str(path), "--summary", str(summary))
        test.assertEqual((rc, err), (0, ""))
        test.assertIn("Selected 0 configs (changed):", out)
        test.assertIn("NO_TARGETS(skip)", out)
        test.assertNotIn("broken=", out)
        test.assertNotIn("| cfg |", out)
        report = json.loads(path.read_text())
        test.assertEqual((report["shard_index"], report["shard_count"]), (index, 4))
        test.assertEqual((report["selected"], report["results"], report["problems"]), ([], [], []))
        checked.assert_not_called()

    aggregate = ("--aggregate-reports", str(reports), "--shard-count", "4", "--summary", str(summary))
    rc, out, err, checked = test.run_main(*aggregate)
    test.assertEqual((rc, err), (0, ""))
    test.assertIn("NO_TARGETS(skip)", out)
    test.assertNotIn("PASS", out + summary.read_text())
    test.assertEqual(summary.read_text().count("NO_TARGETS(skip)"), 5)
    checked.assert_not_called()

    rc, out, err, _ = test.run_main(*aggregate, "--upstream-result", "failure")
    test.assertEqual(rc, 1)
    test.assertIn("TLC matrix result", err)
    test.assertNotIn("NO_TARGETS(skip)", out + err)

    missing = test.root / "M.cfg"
    original = missing.read_bytes()
    missing.unlink()
    try:
        for args in ((), aggregate):
            rc, out, err, _ = test.run_main(*args)
            test.assertEqual(rc, 1)
            test.assertIn("M.cfg: listed in expected.tsv but the file is gone", err)
            test.assertNotIn("NO_TARGETS(skip)", out + err)
    finally:
        missing.write_bytes(original)

    (reports / "tlc-3.json").unlink()
    rc, out, err, _ = test.run_main(*aggregate)
    test.assertEqual(rc, 1)
    test.assertIn("shard coverage mismatch", err)
    test.assertNotIn("NO_TARGETS(skip)", out + err)


def assert_non_model_paths_skip(test) -> None:
    fixture_path = "tests/fixtures/list_merge/delete-modify/ours/expected.tsv"
    fixture = test.repo / fixture_path
    fixture.parent.mkdir(parents=True)
    fixture.write_text("original\n", encoding="utf-8")
    test.git("add", "--", fixture_path)
    test.git("commit", "-qm", "list merge fixture")
    test.base = test.git("rev-parse", "HEAD").strip()

    readme_path = "docs/statecharts/README.md"
    (test.repo / readme_path).write_text("documentation change\n", encoding="utf-8")
    fixture.write_text("changed\n", encoding="utf-8")
    test.git("add", "--", readme_path, fixture_path)
    test.git("commit", "-qm", "non-model changes")
    test.assertCountEqual(test.git("diff", "--name-only", f"{test.base}..HEAD").splitlines(),
                          [readme_path, fixture_path])

    reports = empty_reports(test, 1, "non-model-reports")
    for mode, args in (("check", ()), ("coverage", ("--aggregate-reports", str(reports)))):
        with test.subTest(mode=mode):
            rc, out, err, checked = test.run_main(*args)
            test.assertEqual((rc, err), (0, ""))
            test.assertIn("NO_TARGETS(skip)", out)
            test.assertNotIn("PASS", out)
            checked.assert_not_called()


def assert_model_change_failures(test, tlc) -> None:
    reports = empty_reports(test, 1)
    cases = {
        "Unselected.tla": "---- MODULE Unselected ----\n====\n",
        "M.cfg": "INIT Init\nNEXT Next\n\\* Changed config\n",
        "expected.tsv": test.expected.read_text() + "# Changed comment\n",
    }
    for name, content in cases.items():
        with test.subTest(path=name):
            test.base = test.git("rev-parse", "HEAD").strip()
            (test.root / name).write_text(content, encoding="utf-8")
            test.commit()
            # Simulate a selection regression for a changed, registered config.
            selection = mock.patch.object(tlc, "select_configs", return_value=[]) if name.endswith(".cfg") else nullcontext()
            with selection:
                for mode, args in (("check", ()), ("coverage", ("--aggregate-reports", str(reports)))):
                    with test.subTest(mode=mode):
                        rc, out, err, checked = test.run_main(*args)
                        test.assertEqual(rc, 1)
                        test.assertIn("FAIL NO_TARGETS:", err)
                        test.assertNotRegex(out + err, r"(?m)^NO_TARGETS")
                        checked.assert_not_called()


def assert_empty_full_fallback(test) -> None:
    for path in test.root.glob("*.cfg"):
        path.unlink()
    test.expected.write_text("", encoding="utf-8")
    rc, out, err, checked = test.run_main()
    test.assertEqual(rc, 1)
    test.assertIn("FULL_FALLBACK", err)
    test.assertIn("FAIL NO_TARGETS:", err)
    test.assertNotRegex(out + err, r"(?m)^NO_TARGETS")
    checked.assert_not_called()
