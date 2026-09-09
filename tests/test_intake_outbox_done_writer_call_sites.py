"""Contract and mutation tests for the #5071 T2 intake-outbox done-writer gate."""

from __future__ import annotations

import importlib.util
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check_intake_outbox_done_writer_call_sites.py"
PROOF_OWNER = "src/services/discord/runtime_bootstrap/intake_delivery_reconciler.rs"
SETTLEMENT_OWNER = "src/services/discord/turn_bridge/intake_settlement.rs"
SPEC = importlib.util.spec_from_file_location("intake_outbox_done_writer_guard", SCRIPT)
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)

CURRENT_EXPECTED_CALL_SITES = {
    "mark_done": {"src/services/cluster/intake_worker.rs": 1},
    "mark_done_from_delivery_proof": {PROOF_OWNER: 1},
    "settle_intake_done_from_receipt": {SETTLEMENT_OWNER: 1},
}


def write(root: Path, rel: str, body: str) -> None:
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")


class SourceContractTests(unittest.TestCase):
    def test_real_tree_passes_and_reports_declared_limits(self):
        ok, message = guard.check(ROOT)
        self.assertTrue(ok, message)
        self.assertIn("3 production sites across 3 symbols", message)
        for limit in ("not Rust parsing", "direct SQL writers are NOT seen", "over-counted"):
            self.assertIn(limit, message)
        self.assertIn(
            f"skipped {len(guard.PINNED_TEST_ONLY_MODULE_FILES)} test files",
            message,
        )

    def test_shared_skip_pin_is_the_only_path_and_count_source(self):
        self.assertIs(
            guard.PINNED_TEST_ONLY_MODULE_FILES,
            guard._SKIP_PIN.PINNED_TEST_ONLY_MODULE_FILES,
        )

    def test_conditional_pin_is_the_t2_done_writer_only(self):
        self.assertEqual(guard.PROOF_OWNER, PROOF_OWNER)
        self.assertEqual(guard.expected_call_sites(ROOT), CURRENT_EXPECTED_CALL_SITES)
        self.assertTrue((ROOT / PROOF_OWNER).is_file())

    def test_scan_root_is_all_of_src(self):
        self.assertEqual(guard.SCAN_ROOT.as_posix(), "src")

    def test_ci_script_checks_runs_the_gate_and_its_tests(self):
        """Check wiring spelling/order, not its own execution.

        When this module runs independently, deletion of the gate command fails
        below. It cannot protect deletion of its own unittest invocation from
        ci-script-checks.sh, because that prevents this test from running there.
        """
        wiring = (ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn("scripts/check_intake_outbox_done_writer_call_sites.py", wiring)
        self.assertIn("tests.test_intake_outbox_done_writer_call_sites", wiring)
        self.assertLess(
            wiring.index("scripts/check_intake_outbox_done_writer_call_sites.py"),
            wiring.index("tests.test_intake_outbox_done_writer_call_sites"),
        )

    def test_allowlisted_symbol_is_imported_by_its_owner_function_file(self):
        worker = (ROOT / "src/services/cluster/intake_worker.rs").read_text(encoding="utf-8")
        self.assertIn("pub(crate) async fn run_intake_worker_tick(", worker)
        self.assertIn("mark_done(pool, row.id, claim_owner)", worker)


class DiscriminationTests(unittest.TestCase):
    def fixture(self, proof: bool = False) -> Path:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        write(
            root,
            "src/services/cluster/intake_worker.rs",
            "use crate::db::intake_outbox::{mark_done, mark_spawned};\n"
            "pub(crate) async fn run_intake_worker_tick(\n"
            "    pool: &PgPool,\n"
            "    http: &Arc<serenity::http::Http>,\n"
            "    shared: &Arc<SharedData>,\n"
            "    token: &str,\n"
            "    target_instance_id: &str,\n"
            "    provider: &str,\n"
            "    claim_owner: &str,\n"
            "    cancel: &AtomicBool,\n"
            ") -> Result<TickOutcome, sqlx::Error> {\n"
            "    let advanced = mark_done(pool, row.id, claim_owner).await?;\n"
            "    Ok(TickOutcome::Processed)\n"
            "}\n",
        )
        if proof:
            write(
                root,
                PROOF_OWNER,
                "use crate::db::intake_outbox_delivery_proof::mark_done_from_delivery_proof;\n"
                "async fn reconcile_in_tx(\n"
                "    connection: &mut PgConnection,\n"
                "    outbox_id: i64,\n"
                "    cutoff: DateTime<Utc>,\n"
                ") -> Result<ReconcileOutcome, sqlx::Error> {\n"
                "    Ok(\n"
                "        if mark_done_from_delivery_proof(connection, outbox_id).await? {\n"
                "            ReconcileOutcome::Done\n"
                "        } else {\n"
                "            ReconcileOutcome::Unchanged\n"
                "        },\n"
                "    )\n"
                "}\n",
            )
        write(
            root,
            SETTLEMENT_OWNER,
            "use crate::db::intake_outbox_delivery_proof::{\n"
            "    IntakeSettlementSource, settle_intake_done_from_receipt,\n"
            "};\n"
            "async fn settle_with_lock_timeout(\n"
            "    pool: &sqlx::PgPool,\n"
            "    outbox_id: i64,\n"
            "    source: IntakeSettlementSource,\n"
            ") -> Result<bool, sqlx::Error> {\n"
            "    let mut transaction = pool.begin().await?;\n"
            "    let won = settle_intake_done_from_receipt(&mut transaction, outbox_id, source).await?;\n"
            "    transaction.commit().await?;\n"
            "    Ok(won)\n"
            "}\n",
        )
        return root

    def run_guard(self, root: Path, expected=None) -> tuple[bool, str]:
        return guard.check(root, expected, pinned_test_only_files=frozenset())

    def test_baseline_fixture_is_green(self):
        ok, message = self.run_guard(self.fixture())
        self.assertTrue(ok, message)

    def test_script_process_exit_code_maps_pass_and_failure(self):
        passing = subprocess.run(
            [sys.executable, str(SCRIPT)],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(passing.returncode, 0, passing.stderr)

        root = self.fixture()
        copied_script = root / "scripts/check_intake_outbox_done_writer_call_sites.py"
        copied_script.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(SCRIPT, copied_script)
        shutil.copy2(
            ROOT / "scripts/test_only_module_skip_pin.py",
            root / "scripts/test_only_module_skip_pin.py",
        )
        shutil.copy2(
            ROOT / "scripts/generate_inventory_docs.py",
            root / "scripts/generate_inventory_docs.py",
        )
        shutil.copy2(
            ROOT / "scripts/rust_lex.py",
            root / "scripts/rust_lex.py",
        )
        declarations = []
        for index, rel in enumerate(sorted(guard.PINNED_TEST_ONLY_MODULE_FILES)):
            write(root, rel, "")
            declarations.append(
                f'#[cfg(test)]\n#[path = "{rel.removeprefix("src/")}"]\n'
                f"mod pinned_skip_{index};"
            )
        write(root, "src/pinned_skip_owner.rs", "\n".join(declarations))
        write(
            root,
            "src/services/cluster/receipt_sink.rs",
            "use crate::db::intake_outbox::mark_done;\n"
            "fn receipt() { mark_done(); }\n",
        )
        failing = subprocess.run(
            [sys.executable, str(copied_script)],
            cwd=root,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotEqual(failing.returncode, 0, failing.stdout + failing.stderr)
        self.assertIn("UNLISTED call site", failing.stderr)

    def test_allowlist_entry_deleted_is_fail_closed(self):
        expected = {**CURRENT_EXPECTED_CALL_SITES, "mark_done": {}}
        ok, message = self.run_guard(self.fixture(), expected)
        self.assertFalse(ok)
        self.assertIn("mark_done: UNLISTED call site", message)

        ok, message = self.run_guard(self.fixture(), {})
        self.assertFalse(ok)
        self.assertIn("mark_done: UNLISTED call site", message)

    def test_unlisted_writer_is_fail_closed(self):
        root = self.fixture()
        write(
            root,
            "src/services/cluster/receipt_sink.rs",
            "use crate::db::intake_outbox;\n"
            "fn receipt() { intake_outbox::mark_done(); }\n",
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn(
            "mark_done: UNLISTED call site in src/services/cluster/receipt_sink.rs (1x)",
            message,
        )

    def test_allowlist_path_typo_is_fail_closed(self):
        typo = {"mark_done": {"src/services/cluster/intake_wroker.rs": 1}}
        ok, message = self.run_guard(self.fixture(), typo)
        self.assertFalse(ok)
        self.assertIn("call site GONE from src/services/cluster/intake_wroker.rs", message)
        self.assertIn("UNLISTED call site in src/services/cluster/intake_worker.rs", message)

    def test_cfg_test_writer_call_is_not_a_production_site(self):
        root = self.fixture()
        write(
            root,
            "src/services/cluster/test_only.rs",
            "#[cfg(test)]\nmod tests { fn probe() { crate::db::intake_outbox::mark_done(); } }\n"
            "#[cfg(all(test, unix))]\nfn probe_all() { crate::db::intake_outbox::mark_done(); }\n"
            "#[cfg(test)]\nmod settlement_tests {\n"
            "    async fn probe() {\n"
            "        crate::db::intake_outbox_delivery_proof::settle_intake_done_from_receipt(\n"
            "            &mut transaction, id, IntakeSettlementSource::Committed,\n"
            "        ).await;\n"
            "    }\n"
            "}\n",
        )
        ok, message = self.run_guard(root)
        self.assertTrue(ok, message)

    def test_receipt_writer_wiring_deletion_is_fail_closed(self):
        root = self.fixture()
        write(
            root,
            SETTLEMENT_OWNER,
            "use crate::db::intake_outbox_delivery_proof::{\n"
            "    IntakeSettlementSource, settle_intake_done_from_receipt,\n"
            "};\n"
            "async fn settle_with_lock_timeout(\n"
            "    pool: &sqlx::PgPool,\n"
            "    outbox_id: i64,\n"
            "    source: IntakeSettlementSource,\n"
            ") -> Result<bool, sqlx::Error> {\n"
            "    let mut transaction = pool.begin().await?;\n"
            "    transaction.commit().await?;\n"
            "    Ok(false)\n"
            "}\n",
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn("settle_intake_done_from_receipt: call site GONE", message)

    def test_production_possible_cfg_forms_are_scanned(self):
        for body in (
            "#[cfg(any(test, unix))] fn x(){crate::db::intake_outbox_delivery_proof::mark_done_from_delivery_proof();}",
            "#[cfg_attr(test, allow(dead_code))] fn x(){crate::db::intake_outbox_delivery_proof::mark_done_from_delivery_proof();}",
        ):
            root = self.fixture()
            write(root, "src/services/discord/cfg_writer.rs", body)
            ok, message = self.run_guard(root)
            self.assertFalse(ok)
            self.assertIn("mark_done_from_delivery_proof: UNLISTED call site", message)

    def test_lifetime_does_not_hide_later_fully_qualified_writer(self):
        root = self.fixture()
        write(
            root,
            "src/services/session_backend.rs",
            "fn receipt<'a>() { crate::db::intake_outbox::mark_done(); }\n",
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn(
            "mark_done: UNLISTED call site in src/services/session_backend.rs (1x)",
            message,
        )

    def test_future_proof_writer_activates_and_mutations_fail_closed(self):
        root = self.fixture(proof=True)
        ok, message = self.run_guard(root)
        self.assertTrue(ok, message)
        self.assertIn("3 production sites across 3 symbols", message)

        write(
            root,
            PROOF_OWNER,
            "use crate::db::intake_outbox_delivery_proof::mark_done_from_delivery_proof;\n"
            "fn reconcile() {}\n",
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn("mark_done_from_delivery_proof: call site GONE", message)

        for body in (
            "use crate::db::intake_outbox_delivery_proof::mark_done_from_delivery_proof; fn x(){mark_done_from_delivery_proof();}",
            "fn x(){crate::db::intake_outbox_delivery_proof::mark_done_from_delivery_proof();}",
        ):
            root = self.fixture(proof=True)
            write(root, "src/services/discord/unlisted.rs", body)
            ok, message = self.run_guard(root)
            self.assertFalse(ok)
            self.assertIn("mark_done_from_delivery_proof: UNLISTED call site", message)

        root = self.fixture(proof=True)
        source = root / PROOF_OWNER
        moved = source.with_name("intake_delivery_reconciler_typo.rs")
        source.rename(moved)
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn("mark_done_from_delivery_proof: UNLISTED call site", message)

    def test_proof_import_without_call_and_aliases_fail_closed(self):
        for body, problem in (
            (
                "use crate::db::intake_outbox_delivery_proof::mark_done_from_delivery_proof; fn x(){}",
                "UNLISTED protected import",
            ),
            (
                "use crate::db::intake_outbox_delivery_proof::mark_done_from_delivery_proof as finish; fn x(){finish();}",
                "ALIASED protected import",
            ),
        ):
            root = self.fixture()
            write(root, "src/services/discord/unlisted.rs", body)
            ok, message = self.run_guard(root)
            self.assertFalse(ok)
            self.assertIn(problem, message)

        root = self.fixture(proof=True)
        write(
            root,
            PROOF_OWNER,
            "use crate::db::intake_outbox_delivery_proof::mark_done_from_delivery_proof as finish; fn x(){finish();}",
        )
        ok, message = self.run_guard(root)
        self.assertFalse(ok)
        self.assertIn("ALIASED protected import", message)

    def test_proof_owner_constant_mutation_is_not_followed_by_fixture(self):
        root = self.fixture(proof=True)
        original = guard.PROOF_OWNER
        guard.PROOF_OWNER = "src/services/discord/runtime_bootstrap/alternate_absent.rs"
        try:
            ok, message = self.run_guard(root)
        finally:
            guard.PROOF_OWNER = original
        self.assertFalse(ok)
        self.assertIn(
            f"mark_done_from_delivery_proof: UNLISTED protected import in {PROOF_OWNER}",
            message,
        )


class TransportLegacyInventoryTests(unittest.TestCase):
    """#5485 S2a: read-only shutdown transport + seven legacy writer adapters.

    The `legacy_*` spellings are a convention the compiler cannot enforce, so
    these enumerated assertions are the entire S2a enforcement surface (design
    `adapters.ownership_boundary`); field narrowing is S2b's job.
    """

    WORKER = ROOT / "src/services/cluster/intake_worker.rs"
    OWNER = ROOT / "src/services/discord/shared_state.rs"
    BOOTSTRAP = ROOT / "src/services/discord/runtime_bootstrap"
    RAW_STORE = re.compile(r"(shutting_down|restart_pending)\s*\.store\(")
    ADAPTER_STORE = re.compile(
        r"self\.(shutting_down|restart_pending)\.store\((\w+), Ordering::SeqCst\)"
    )
    RAW_WRITER_CLONE = re.compile(
        r"restart\.shutting_down\.clone\(\)|Arc::clone\(&[\w.]*restart\.shutting_down\)"
    )
    # Every `impl ShutdownReader`/`impl <Trait> for ShutdownReader` item head.
    READER_IMPL = re.compile(
        r"^impl(?:<[^>]*>)?\s+(?:(?P<trait>[^\s{]+)\s+for\s+)?ShutdownReader\b[^\n{]*\{", re.M
    )
    READER_DECL = re.compile(r"struct ShutdownReader\((?P<field>.*)\);")
    RAW_HANDLE = re.compile(r"self\.0(?!\.load\()")
    CHECKPOINT = re.compile(
        r"admission_action\(\s*cancelled,\s*&shared\.restart\.intake_worker_lifecycle,"
        r"\s*AdmissionCheckpoint::(\w+),"
    )
    # "<adapter> <caller basename> <field>=<value>..." in exact store order.
    ADAPTERS = """
        legacy_deferred_begin deferred_restart.rs shutting_down=true
        legacy_deferred_ack deferred_restart.rs restart_pending=true
        legacy_deferred_rollback deferred_restart.rs shutting_down=false restart_pending=false
        legacy_promotion_fence gateway_lease_recovery.rs restart_pending=true
        legacy_promotion_unfence gateway_lease_recovery.rs restart_pending=false
        legacy_lease_lost gateway_lease.rs shutting_down=true restart_pending=true
        legacy_sigterm shutdown.rs shutting_down=true restart_pending=true
    """

    def item(self, text: str, signature: str) -> str:
        start = text.index(signature)
        return text[start : text.index("\n}\n", start)]

    def test_transport_legacy_lexical_inventory(self):
        worker = self.WORKER.read_text(encoding="utf-8")
        owner = self.OWNER.read_text(encoding="utf-8")

        tick = self.item(worker, "pub(crate) async fn run_intake_worker_tick(")
        self.assertIn("cancelled: &(dyn Fn() -> bool + Sync),", tick)
        self.assertEqual(
            self.CHECKPOINT.findall(tick), ["BeforeClaim", "AfterClaim", "AfterClaim"]
        )
        self.assertEqual(tick.count("admission_action("), 3)
        self.assertEqual(tick.count("release_cancelled_claim(pool, &row, claim_owner)"), 2)

        loop_body = self.item(worker, "pub(crate) async fn run_intake_worker_loop(")
        self.assertNotIn("cancel: Arc<AtomicBool>", loop_body)
        self.assertIn("let reader = shared.restart.shutdown_reader();", loop_body)
        self.assertIn("let cancelled = || reader.load(Ordering::Acquire);", loop_body)
        self.assertEqual(loop_body.count("if cancelled() {"), 2)
        self.assertIn("&cancelled,", loop_body)

        escapes = [
            str(path) for path in sorted((ROOT / "src").rglob("*.rs"))
            if self.RAW_WRITER_CLONE.search(path.read_text(encoding="utf-8"))
        ]
        self.assertEqual(escapes, [], "four raw writer-capability escapes stay replaced")

        self.assert_reader_surface(owner)

        stores = 0
        rows = [line.split() for line in self.ADAPTERS.strip().splitlines()]
        for name, caller_file, *spec in rows:
            expected = [tuple(pair.split("=")) for pair in spec]
            body = self.item(owner, f"fn {name}(&self) {{").split("\n    }")[0]
            self.assertEqual(self.ADAPTER_STORE.findall(body), expected, name)
            self.assertEqual(owner.count(f"fn {name}(&self)"), 1, name)
            caller = (self.BOOTSTRAP / caller_file).read_text(encoding="utf-8")
            self.assertEqual(caller.count(f".{name}();"), 1, name)
            self.assertEqual(self.RAW_STORE.findall(caller), [], caller_file)
            stores += len(expected)
        self.assertEqual((len(rows), stores), (7, 10))

    def assert_reader_surface(self, owner: str) -> None:
        """Audit every `impl ... ShutdownReader`; first-block-only was #5831 P1-1."""
        code = "\n".join(l for l in owner.splitlines() if not l.lstrip().startswith("//"))
        decl = self.READER_DECL.search(code)
        self.assertIsNotNone(decl, "ShutdownReader tuple struct declaration")
        self.assertNotIn("pub", decl.group("field"), "wrapped handle stays a private field")
        inherent = []
        for header in self.READER_IMPL.finditer(code):
            head = header.group(0)
            body = code[header.end() : code.index("\n}\n", header.end())]
            self.assertEqual(self.RAW_HANDLE.findall(body), [], f"raw handle escapes {head}")
            self.assertNotIn("Arc<", body, f"writable handle re-exposed by {head}")
            if header.group("trait") is None:
                inherent.append(body)
        self.assertEqual(len(inherent), 1, "exactly one inherent impl ShutdownReader")
        self.assertEqual(inherent[0].count("fn "), 1)
        self.assertIn("fn load(&self, order: Ordering) -> bool {", inherent[0])
        for forbidden in ("fn store", "fn swap", "impl Deref", "impl AsRef", "impl From<"):
            self.assertNotIn(forbidden, code, f"ShutdownReader must not gain {forbidden}")
        self.assertEqual(code.count("ShutdownReader(self.shutting_down.clone())"), 1)

    def mutate(self, old: str, new: str) -> str:
        owner = self.OWNER.read_text(encoding="utf-8")
        self.assertIn(old, owner)
        return owner.replace(old, new, 1)

    def test_reader_surface_is_red_when_a_second_impl_adds_a_raw_accessor(self):
        with self.assertRaises(AssertionError):
            self.assert_reader_surface(
                self.mutate(
                    "impl RestartLifecycle {",
                    "impl ShutdownReader {\n"
                    "    pub(in crate::services) fn raw_handle(&self) -> Arc<AtomicBool> {\n"
                    "        self.0.clone()\n"
                    "    }\n"
                    "}\n\nimpl RestartLifecycle {",
                )
            )

    def test_reader_surface_is_red_when_the_wrapped_handle_field_goes_public(self):
        with self.assertRaises(AssertionError):
            self.assert_reader_surface(
                self.mutate(
                    "struct ShutdownReader(Arc<",
                    "struct ShutdownReader(pub(in crate::services) Arc<",
                )
            )


if __name__ == "__main__":
    unittest.main()
