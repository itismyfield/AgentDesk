"""CLI-boundary tests for the B3b-4 dormancy gate: a clean tree passes, each planted leak fails."""
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts/check_b3b4_dormancy.py"
PURGE = "src/services/turn_orchestrator/registry_purge.rs"
REGISTRY = "src/services/turn_orchestrator/recovery_registry/mod.rs"
HOME_PORT = "src/services/cluster/channel_home_port.rs"
TREE = {
    "Cargo.toml": "[features]\ndefault = []\n",
    "migrations/postgres/0140_channel_recovery_custody.sql":
        "-- additive\nCREATE TABLE channel_recovery_custody (channel_id TEXT PRIMARY KEY);\n",
    "src/db/mod.rs": "#[cfg(test)]\npub(crate) mod channel_recovery_custody;\n",
    "src/db/channel_recovery_custody.rs": "pub(crate) struct StoredCustody;\n",
    "src/db/o_channel_homes.rs": "#[cfg(test)]\npub(crate) async fn read_home_in(\n) {}\n",
    "src/services/agent_recovery/admission.rs": "#[cfg(test)]\nimpl AdmissionGuard {\n}\n",
    HOME_PORT: (
        "#[cfg(all(test, unix))]\npub(crate) mod scoped_restore {\n    pub(crate) enum RestoreStatus {\n"
        "        Installed,\n    }\n    pub(crate) struct AttemptIdentity;\n"
        "    pub(crate) trait ExactWitness {}\n}\n"),
    PURGE: (
        '#[cfg(test)]\n#[path = "recovery_custody/mod.rs"]\npub(crate) mod recovery_custody;\n\n'
        '#[cfg(test)]\n#[path = "recovery_registry/mod.rs"]\npub(crate) mod recovery_registry;\n'),
    "src/services/turn_orchestrator/recovery_custody/mod.rs":
        "pub(crate) struct CustodyStore;\n#[cfg(test)]\npub(crate) mod commit_fault {\n}\n"
        "#[cfg(test)]\npub(crate) mod custody_pg_tests;\n",
    "src/services/turn_orchestrator/recovery_custody/custody_pg_tests.rs":
        'fn mutant() -> bool { std::env::var("X").is_ok() }\n',
    REGISTRY: (
        "pub(crate) struct RecoveryRegistry;\nimpl RecoveryRegistry {\n    #[cfg(test)]\n"
        "    pub(crate) fn isolated() {}\n}\n"),
    "src/services/turn_orchestrator.rs": "pub(crate) struct ChannelMailboxRegistry {\n    handles: u8,\n}\n",
}


def run(tree, *args):
    with tempfile.TemporaryDirectory() as tmp:
        for name, text in tree.items():
            path = Path(tmp) / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        done = subprocess.run([sys.executable, str(SCRIPT), "--root", tmp, *args],
                              capture_output=True, text=True)
        return done.returncode, done.stdout


def planted(path, old, new):
    assert old in TREE[path], (path, old)
    return {**TREE, path: TREE[path].replace(old, new)}


class DormancyGate(unittest.TestCase):
    def assert_fails(self, tree, needle):
        code, out = run(tree)
        self.assertEqual(code, 1, out)
        self.assertIn(needle, out)

    def test_clean_tree_and_the_real_repository_pass(self):
        self.assertEqual(run(TREE), (0, "B3b-4 dormancy: PASS\n"))
        done = subprocess.run([sys.executable, str(SCRIPT)], capture_output=True, text=True)
        self.assertEqual((done.returncode, done.stdout), (0, "B3b-4 dormancy: PASS\n"))

    def test_an_ungated_or_loosely_gated_declaration_fails(self):
        gate = '#[cfg(test)]\n#[path = "recovery_registry/mod.rs"]'
        self.assert_fails(planted(PURGE, gate, '#[path = "recovery_registry/mod.rs"]'),
                          "declaration not under #[cfg(test)]")
        for loose in ('#[cfg(any(test, feature = "b3b4"))]', '#[cfg_attr(test, allow(dead_code))]\n#[cfg(test)]'):
            tree = planted(PURGE, gate, loose + '\n#[path = "recovery_registry/mod.rs"]')
            self.assertEqual(run(tree)[0], 1, loose)
        self.assert_fails(planted("src/db/mod.rs", "#[cfg(test)]\n", ""), "declaration not under #[cfg(test)]")

    def test_an_alias_a_field_or_a_call_outside_the_bodies_fails(self):
        mailbox = "src/services/turn_orchestrator.rs"
        leaks = (
            "pub(crate) use registry_purge::recovery_registry as recovery;\n",
            "pub(crate) type Recovery = registry_purge::recovery_registry::RecoveryRegistry;\n",
            "    handles: u8,\n    recovery: registry_purge::recovery_registry::RecoveryRegistry,\n",
            "fn boot() { registry_purge::recovery_custody::CustodyStore::new(); }\n",
            "#[cfg(test)]\nuse registry_purge::recovery_registry::RecoveryRegistry;\n",
        )
        for leak in leaks:
            tree = {**TREE, mailbox: TREE[mailbox].replace("    handles: u8,\n", leak, 1)
                    if leak.startswith("    handles") else leak + TREE[mailbox]}
            self.assert_fails(tree, "reference to a dormant module outside it")

    def test_a_fixture_or_a_switch_exposed_in_a_body_fails(self):
        self.assert_fails(planted(REGISTRY, "    #[cfg(test)]\n", ""), "test fixture without #[cfg(test)]")
        for switch in ('#[cfg(feature = "open")]\n', "#[allow(dead_code)]\n",
                       'fn on() -> bool { std::env::var("OPEN").is_ok() }\n'):
            tree = {**TREE, REGISTRY: switch + TREE[REGISTRY]}
            self.assert_fails(tree, "gate or switch not allowed in a dormant body")
        self.assert_fails({**TREE, "Cargo.toml": "[features]\nrecovery_registry = []\n"}, "Cargo.toml")

    def test_a_seam_or_a_scoped_type_outside_its_test_gate_fails(self):
        self.assert_fails(planted("src/db/o_channel_homes.rs", "#[cfg(test)]\n", ""), "test seam")
        self.assert_fails(planted(HOME_PORT, "#[cfg(all(test, unix))]", "#[cfg(unix)]"), "test seam")
        outside = {**TREE, HOME_PORT: TREE[HOME_PORT] + "pub(crate) struct AttemptIdentity;\n"}
        self.assert_fails(outside, "scoped-restore type outside its test block")

    def test_a_migration_that_touches_existing_data_fails(self):
        path = "migrations/postgres/0140_channel_recovery_custody.sql"
        self.assert_fails({**TREE, path: TREE[path] + "ALTER TABLE o_channel_homes ADD COLUMN x INT;\n"},
                          "not additive")


if __name__ == "__main__":
    unittest.main()
