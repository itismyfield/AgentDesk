import importlib.util
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
spec = importlib.util.spec_from_file_location("n1_boundary", ROOT / "scripts/check_n1_observation_boundary.py")
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)


class ObservationBoundaryTests(unittest.TestCase):
    def test_production_producer_passes(self):
        self.assertEqual(guard.producer_errors((ROOT / guard.PRODUCER).read_text()), [])

    def test_effect_and_blocking_mutants_are_red(self):
        source = (ROOT / guard.PRODUCER).read_text()
        for mutation in (
            source.replace(".try_send(", ".send(", 1),
            source.replace(".try_lock(", ".lock(", 1),
            source + "\nuse std::{fs as disk}; fn effect() { disk::write(p, b); }",
            source + "\nfn effect() { crate::db::insert(p); }",
            source + "\nfn effect() { tracing::warn!(\"full\"); }",
            source + "\nfn effect() { tokio::spawn(work); }",
            source + "\nfn effect() { let locker = Mutex::lock; locker(m); }",
            source + "\nuse super::shadow::root::ShadowRoot; fn effect() { ShadowRoot::under(p); }",
            source + "\nfn effect() { sink::initialize(p); }",
        ):
            with self.subTest(mutation=mutation[-70:]):
                self.assertTrue(guard.producer_errors(mutation))
