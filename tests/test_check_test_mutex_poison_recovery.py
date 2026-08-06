"""Hermetic fixtures for the process-global `Mutex<()>` poison gate (#5185).

Every case here is a mutation of a *passing* fixture: the passing shape is
asserted first so that a detector which reports everything, or nothing, fails
these tests rather than sitting green next to a broken scan.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts/check_test_mutex_poison_recovery.py"


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


gate = load_module("check_test_mutex_poison_recovery", SCRIPT)

RECOVERED = """
static SHARED_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    #[test]
    fn case() {
        let _guard = super::SHARED_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
    }
}
"""


class GateCase(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "src").mkdir()

    def write(self, source: str, name: str = "src/service.rs") -> None:
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(source, "utf-8")

    def run_gate(self) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = gate.check(self.root, self.root / "src")
        return rc, out.getvalue(), err.getvalue()


class PropagationMutations(GateCase):
    def test_recovered_passes_and_unwrap_fails(self) -> None:
        self.write(RECOVERED)
        self.assertEqual(self.run_gate()[0], 0)
        self.write(
            RECOVERED.replace(".unwrap_or_else(|poison| poison.into_inner())", ".unwrap()")
        )
        rc, _, error = self.run_gate()
        self.assertEqual(rc, 1)
        self.assertIn("unwrap()", error)
        self.assertIn("src/service.rs", error)

    def test_expect_and_question_mark_are_caught(self) -> None:
        # `expect` is the spelling the first repair round left behind at
        # `shared_test_env_lock` sites; it panics on PoisonError identically.
        for replacement, label in (
            ('.expect("poisoned")', "expect(..)"),
            ("?", "?"),
        ):
            with self.subTest(mutation=replacement):
                self.write(
                    RECOVERED.replace(
                        ".unwrap_or_else(|poison| poison.into_inner())", replacement
                    )
                )
                rc, _, error = self.run_gate()
                self.assertEqual(rc, 1)
                self.assertIn(label, error)

    def test_unwrap_or_else_without_into_inner_is_not_recovery(self) -> None:
        self.write(
            RECOVERED.replace(
                ".unwrap_or_else(|poison| poison.into_inner())",
                '.unwrap_or_else(|_| panic!("poisoned"))',
            )
        )
        rc, _, error = self.run_gate()
        self.assertEqual(rc, 1)
        self.assertIn("unwrap_or_else(..) without into_inner", error)

    def test_path_form_recovery_is_accepted(self) -> None:
        self.write(
            RECOVERED.replace(
                ".unwrap_or_else(|poison| poison.into_inner())",
                ".unwrap_or_else(std::sync::PoisonError::into_inner)",
            )
        )
        self.assertEqual(self.run_gate()[0], 0)

    def test_match_form_recovery_is_accepted(self) -> None:
        self.write(
            RECOVERED.replace(
                """.lock()
            .unwrap_or_else(|poison| poison.into_inner());""",
                """.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };""",
            ).replace("let _guard = super::SHARED_TEST_LOCK", "let _guard = match super::SHARED_TEST_LOCK")
        )
        self.assertEqual(self.run_gate()[0], 0)

    def test_commented_out_propagation_is_not_a_site(self) -> None:
        self.write(
            RECOVERED
            + "\n// Historical shape: SHARED_TEST_LOCK.lock().unwrap();\n"
        )
        self.assertEqual(self.run_gate()[0], 0)


class InventoryScope(GateCase):
    def test_accessor_returning_static_unit_mutex_is_covered(self) -> None:
        self.write(
            """
fn shared_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[test]
fn case() {
    let _guard = shared_env_lock().lock().unwrap();
}
"""
        )
        rc, _, error = self.run_gate()
        self.assertEqual(rc, 1)
        self.assertIn("shared_env_lock.lock().unwrap()", error)

    def test_tokio_mutex_is_out_of_scope(self) -> None:
        """`.lock().await` cannot be poisoned; flagging it would be pure noise."""
        self.write(RECOVERED)
        self.write(
            """
fn auto_heal_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

static ASYNC_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

#[tokio::test]
async fn case() {
    let _a = auto_heal_test_lock().lock().await;
    let _b = ASYNC_LOCK.lock().await;
}
""",
            "src/async_lane.rs",
        )
        rc, out, _ = self.run_gate()
        self.assertEqual(rc, 0)
        self.assertNotIn("auto_heal_test_lock", out)
        self.assertNotIn("ASYNC_LOCK", out)

    def test_guarded_state_mutex_is_out_of_scope(self) -> None:
        """`Mutex<HashMap<..>>` recovery is a decision about the value."""
        self.write(RECOVERED)
        self.write(
            """
static STATE: std::sync::Mutex<std::collections::HashMap<String, u64>> =
    std::sync::Mutex::new(std::collections::HashMap::new());

#[test]
fn case() {
    let _guard = STATE.lock().unwrap();
}
""",
            "src/state.rs",
        )
        rc, out, _ = self.run_gate()
        self.assertEqual(rc, 0)
        self.assertNotIn("STATE", out)

    def test_per_key_mutex_map_is_not_a_process_global(self) -> None:
        self.write(RECOVERED)
        self.write(
            """
static SESSION_TURN_LOCKS: std::sync::LazyLock<dashmap::DashMap<String, Arc<Mutex<()>>>> =
    std::sync::LazyLock::new(dashmap::DashMap::new);
""",
            "src/per_key.rs",
        )
        rc, out, _ = self.run_gate()
        self.assertEqual(rc, 0)
        self.assertNotIn("SESSION_TURN_LOCKS", out)

    def test_empty_inventory_is_a_broken_scan_not_a_clean_tree(self) -> None:
        self.write("pub fn nothing() {}\n")
        rc, _, error = self.run_gate()
        self.assertEqual(rc, 2)
        self.assertIn("broken scan", error)

    def test_inventory_is_printed_so_the_covered_set_is_reviewable(self) -> None:
        self.write(RECOVERED)
        rc, out, _ = self.run_gate()
        self.assertEqual(rc, 0)
        self.assertIn("SHARED_TEST_LOCK: src/service.rs", out)


class RealTreeContract(unittest.TestCase):
    def test_repository_tree_is_clean_and_the_inventory_is_non_trivial(self) -> None:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = gate.check(REPO_ROOT, REPO_ROOT / "src")
        self.assertEqual(rc, 0, err.getvalue())
        inventory = gate.discover_inventory(
            REPO_ROOT, sorted((REPO_ROOT / "src").rglob("*.rs"))
        )
        # A detector that silently stopped matching would still report "0
        # propagating acquisitions". Pin that it is looking at a real inventory,
        # and at the two mutexes whose cascades this gate exists to prevent.
        self.assertGreaterEqual(len(inventory), 20)
        self.assertIn("shared_test_env_lock", inventory)
        self.assertIn("TEST_LOCK", inventory)


if __name__ == "__main__":
    unittest.main()
