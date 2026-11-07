"""The unbounded-wait check of `scripts/ci/rust_test_sources.py check`: test-only `.join()`/`.recv()` fail,
the allowlist only shrinks against its base, and the real tree passes against its current allowlist."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
spec = importlib.util.spec_from_file_location("rust_test_sources", ROOT / "scripts/ci/rust_test_sources.py")
rts = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rts)

LIB = """\
#[cfg(test)]
mod joins_tests;
#[cfg(all(test, unix))]
mod recv_tests;
mod prod;

pub fn production(handle: std::thread::JoinHandle<()>) {
    handle.join().unwrap();
}

#[cfg(test)]
mod tests {
    #[test]
    fn capped() {
        let (tx, rx) = std::sync::mpsc::channel::<u8>();
        drop(tx);
        let _ = crate::test_support::wait::recv_within(&rx, std::time::Duration::from_secs(1));
        let _ = tokio::time::timeout(d, async { rx2.recv().await });
    }
    #[test]
    fn bare_recv() {
        let (_tx, rx) = std::sync::mpsc::channel::<u8>();
        rx.recv().unwrap(); // "a comment with .join()"
    }
}

pub struct Probe {
    #[cfg(test)]
    pub seen: u8,
    pub real: u8,
}

pub fn gather(handle: std::thread::JoinHandle<()>) {
    handle.join().unwrap();
}
"""
JOINS = "#[test]\nfn joins() {\n    std::thread::spawn(|| 1).join().unwrap();\n}\n"
RECVS = "#[test]\nfn recvs() {\n    let (_tx, rx) = std::sync::mpsc::channel::<u8>();\n    let _ = rx.recv();\n}\n"
PROD = "pub fn gather(handle: std::thread::JoinHandle<()>) {\n    handle.join().unwrap();\n}\n"
INTEGRATION = "#[test]\nfn integration() {\n    std::thread::spawn(|| 1).join().expect(\"ok\");\n}\n"
ALLOW_ALL = "src/joins_tests.rs\nsrc/lib.rs\nsrc/recv_tests.rs\ntests/smoke.rs\n"

# (production-module source, lines the check must report; () means the file must pass)
REGION_CASES = {
    "parameter gate ends at its comma": (
        "pub fn observe(#[cfg(test)] probe: Option<u8>, rx: std::sync::mpsc::Receiver<u8>) {\n"
        "    rx.recv().unwrap();\n}\n", ()),
    "parameter gate ends at its paren": (
        "pub fn observe(rx: std::sync::mpsc::Receiver<u8>, #[cfg(test)] probe: Option<u8>) {\n"
        "    rx.recv().unwrap();\n}\n", ()),
    "test-only fn with generic commas": (
        "#[cfg(test)]\nfn only<A, B>(h: std::thread::JoinHandle<Result<(), A>>) where A: Send, B: Send {\n"
        "    h.join().unwrap();\n}\n", (3,)),
    "direct #[test] in a production module": (
        "pub fn real(h: std::thread::JoinHandle<()>) { h.join().unwrap(); }\n"
        "#[test]\nfn t() { let (_tx, rx) = std::sync::mpsc::channel::<u8>(); rx.recv().unwrap(); }\n"
        "#[tokio::test]\nasync fn a() { let (_tx, mut rx) = tokio::sync::mpsc::channel::<u8>(1); rx.recv().await; }\n",
        (3, 5)),
    "inner cfg block": ("mod tests {\n    #![cfg(test)]\n    fn t(h: std::thread::JoinHandle<()>) { h.join().unwrap(); }\n}\n", (3,)),
    "test-only statement": (
        "pub fn real(rx: std::sync::mpsc::Receiver<u8>) {\n    #[cfg(test)]\n    let seen = rx.recv().unwrap();\n"
        "    let _ = rx.recv_timeout(std::time::Duration::from_secs(1));\n}\n", (3,)),
    "comment inside the call": ("#[cfg(test)]\nfn t(rx: std::sync::mpsc::Receiver<u8>) { rx.recv(/* first */).unwrap(); }\n", (2,)),
    "only comments and strings mention the call": (
        "#[cfg(test)]\nfn t() -> &'static str {\n    // rx.recv()\n    \"h.join()\"\n}\n", ()),
}


def wait_problems(repo: Path) -> list[str]:
    return rts.test_wait_findings(repo, rts.lib_inventory(repo))


def git(repo: Path, *args: str) -> str:
    return subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True, check=True).stdout


class UnboundedWaitCheckTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = Path(self.tmp.name)
        (self.repo / "src").mkdir()
        (self.repo / "scripts/ci").mkdir(parents=True)
        (self.repo / "tests/fixtures").mkdir(parents=True)
        (self.repo / "Cargo.toml").write_text("[package]\nname = \"fixture\"\n", encoding="utf-8")
        (self.repo / "src/lib.rs").write_text(LIB, encoding="utf-8")
        (self.repo / "src/joins_tests.rs").write_text(JOINS, encoding="utf-8")
        (self.repo / "src/recv_tests.rs").write_text(RECVS, encoding="utf-8")
        (self.repo / "src/prod.rs").write_text(PROD, encoding="utf-8")
        (self.repo / "tests/smoke.rs").write_text(INTEGRATION, encoding="utf-8")
        (self.repo / "tests/fixtures/sample.rs").write_text(INTEGRATION, encoding="utf-8")
        self.allowlist = self.repo / rts.WAIT_ALLOWLIST

    def tearDown(self):
        self.tmp.cleanup()

    def test_only_test_code_waits_are_found_and_the_allowlist_passes_them(self):
        found = wait_problems(self.repo)
        sites = sorted(line.split(" ")[2] for line in found)
        self.assertEqual(sites, ["src/joins_tests.rs:3", "src/lib.rs:23", "src/recv_tests.rs:4", "tests/smoke.rs:3"])
        self.assertTrue(all("without a cap" in line for line in found), found)
        self.allowlist.write_text("# rows leave only\n" + ALLOW_ALL, encoding="utf-8")
        self.assertEqual(wait_problems(self.repo), [])

    def test_test_only_regions_follow_cfg_test_and_test_attributes(self):
        for name, (source, lines) in REGION_CASES.items():
            with self.subTest(name):
                (self.repo / "src/prod.rs").write_text(source, encoding="utf-8")
                found = [line for line in wait_problems(self.repo) if "src/prod.rs" in line]
                self.assertEqual([int(line.split(" ")[2].rsplit(":", 1)[1]) for line in found], list(lines), found)

    def test_a_new_bare_join_in_a_test_file_fails(self):
        self.allowlist.write_text(ALLOW_ALL, encoding="utf-8")
        (self.repo / "src/lib.rs").write_text(LIB + "#[cfg(test)]\nmod late_tests;\n", encoding="utf-8")
        (self.repo / "src/late_tests.rs").write_text(JOINS, encoding="utf-8")
        found = wait_problems(self.repo)
        self.assertEqual(len(found), 1, found)
        self.assertIn("unbounded wait: src/late_tests.rs:3 thread join", found[0])

    def test_an_allowlist_row_for_a_clean_file_fails(self):
        self.allowlist.write_text(ALLOW_ALL + "src/prod.rs\n", encoding="utf-8")
        found = wait_problems(self.repo)
        self.assertEqual(len(found), 1, found)
        self.assertIn("src/prod.rs has no unbounded wait left; remove from", found[0])
        self.allowlist.write_text("# fixed in #1234\nsrc/joins_tests.rs\n" + ALLOW_ALL, encoding="utf-8")
        found = wait_problems(self.repo)
        self.assertEqual([line.split(" ", 1)[0] for line in found],
                         [f"{rts.WAIT_ALLOWLIST}:1", f"{rts.WAIT_ALLOWLIST}:3"])

    def test_the_allowlist_only_shrinks_against_its_base(self):
        base = {"src/joins_tests.rs", "src/lib.rs"}
        self.assertEqual(rts.allowlist_growth(["src/lib.rs"], base), [])
        self.assertEqual(rts.allowlist_growth(["src/lib.rs", "src/late_tests.rs"], base),
                         ["unbounded wait allowlist: src/late_tests.rs is new against the base allowlist; "
                          "the allowlist only shrinks, use test_support::wait instead"])
        # A row swapped for a new path keeps the row count and still fails.
        self.assertEqual(len(rts.allowlist_growth(["src/lib.rs", "src/late_tests.rs"], {"src/lib.rs", "src/old.rs"})), 1)
        self.assertEqual(rts.allowlist_growth(["src/lib.rs"], None), [])
        # Through git: the committed allowlist is the base, a new violation plus its row fails the whole check.
        self.allowlist.write_text(ALLOW_ALL, encoding="utf-8")
        git(self.repo, "init", "-q")
        git(self.repo, "-c", "user.email=t@t", "-c", "user.name=t", "add", "-A")
        git(self.repo, "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "base")
        self.assertEqual(rts.check_findings(self.repo, "HEAD")[1], [])
        (self.repo / "src/lib.rs").write_text(LIB + "#[cfg(test)]\nmod late_tests;\n", encoding="utf-8")
        (self.repo / "src/late_tests.rs").write_text(JOINS, encoding="utf-8")
        self.allowlist.write_text(ALLOW_ALL + "src/late_tests.rs\n", encoding="utf-8")
        found = rts.check_findings(self.repo, "HEAD")[1]
        self.assertEqual(len(found), 1, found)
        self.assertIn("src/late_tests.rs is new against the base allowlist", found[0])
        self.assertTrue(any("unreadable" in line for line in rts.check_findings(self.repo, "no-such-rev")[1]))

    def test_the_tree_passes_against_its_allowlist(self):
        try:
            base = git(ROOT, "merge-base", "HEAD", "origin/main").strip()
        except subprocess.CalledProcessError:
            base = "HEAD^1"
        self.assertEqual(rts.check_findings(ROOT, base)[1], [])


if __name__ == "__main__":
    unittest.main()
