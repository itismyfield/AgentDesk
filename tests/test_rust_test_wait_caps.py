"""The unbounded-wait check of `scripts/ci/rust_test_sources.py check`: test-only `.join()`/`.recv()` fail,
the allowlist only shrinks, and the real tree passes against its current allowlist."""

from __future__ import annotations

import importlib.util
from pathlib import Path
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


def wait_problems(repo: Path) -> list[str]:
    return rts.test_wait_findings(repo, rts.lib_inventory(repo))


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
        self.allowlist.write_text("# rows leave only\nsrc/joins_tests.rs\nsrc/lib.rs\nsrc/recv_tests.rs\ntests/smoke.rs\n",
                                  encoding="utf-8")
        self.assertEqual(wait_problems(self.repo), [])

    def test_a_new_bare_join_in_a_test_file_fails(self):
        self.allowlist.write_text("src/joins_tests.rs\nsrc/lib.rs\nsrc/recv_tests.rs\ntests/smoke.rs\n", encoding="utf-8")
        (self.repo / "src/lib.rs").write_text(LIB + "#[cfg(test)]\nmod late_tests;\n", encoding="utf-8")
        (self.repo / "src/late_tests.rs").write_text(JOINS, encoding="utf-8")
        found = wait_problems(self.repo)
        self.assertEqual(len(found), 1, found)
        self.assertIn("unbounded wait: src/late_tests.rs:3 thread join", found[0])

    def test_an_allowlist_row_for_a_clean_file_fails(self):
        self.allowlist.write_text("src/joins_tests.rs\nsrc/lib.rs\nsrc/recv_tests.rs\ntests/smoke.rs\nsrc/prod.rs\n",
                                  encoding="utf-8")
        found = wait_problems(self.repo)
        self.assertEqual(len(found), 1, found)
        self.assertIn("src/prod.rs has no unbounded wait left; remove from", found[0])
        self.allowlist.write_text("# fixed in #1234\nsrc/joins_tests.rs\nsrc/joins_tests.rs\nsrc/lib.rs\nsrc/recv_tests.rs\n"
                                  "tests/smoke.rs\n", encoding="utf-8")
        found = wait_problems(self.repo)
        self.assertEqual([line.split(" ", 1)[0] for line in found],
                         [f"{rts.WAIT_ALLOWLIST}:1", f"{rts.WAIT_ALLOWLIST}:3"])

    def test_the_tree_passes_against_its_allowlist(self):
        self.assertEqual(wait_problems(ROOT), [])


if __name__ == "__main__":
    unittest.main()
