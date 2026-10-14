"""CLI-boundary tests for the B3b-4 dormancy gate: a clean tree passes, each planted leak fails."""
import os
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
MAILBOX = "src/services/turn_orchestrator.rs"
INCARNATION = "src/services/turn_orchestrator/incarnation.rs"
VARIANT = "    #[cfg(test)]\n    Carried(Box<registry_purge::mailbox_transport::Carried>),\n"
REQUEST = "    async fn request<T>(&self) -> Result<T, Unreachable> {\n        self.sender.send(msg)\n    }\n"
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
        '#[cfg(test)]\n#[path = "recovery_registry/mod.rs"]\npub(crate) mod recovery_registry;\n'
        '#[cfg(test)]\n#[path = "recovery_custody/mailbox_access.rs"]\nmod mailbox_access;\n'
        '#[cfg(test)]\n#[path = "recovery_custody/mailbox_transport.rs"]\npub(super) mod mailbox_transport;\n'),
    "src/services/turn_orchestrator/recovery_custody/mod.rs":
        "pub(crate) struct CustodyStore;\n#[cfg(test)]\npub(crate) mod commit_fault {\n}\n"
        "#[cfg(test)]\npub(crate) mod custody_pg_tests;\n",
    "src/services/turn_orchestrator/recovery_custody/custody_pg_tests.rs":
        'fn mutant() -> bool { std::env::var("X").is_ok() }\n',
    REGISTRY: (
        "pub(crate) struct RecoveryRegistry;\nimpl RecoveryRegistry {\n    #[cfg(test)]\n"
        "    pub(crate) fn isolated() {}\n}\n"),
    MAILBOX: ("pub(crate) struct ChannelMailboxRegistry {\n    handles: u8,\n}\nimpl Handle {\n" + REQUEST
              + "}\nenum ChannelMailboxMsg {\n" + VARIANT + "}\n"),
    INCARNATION: "impl Handle {\n}\n",
}


# Each run decides CI mode from its own arguments, not from the environment running the tests.
ENV = {key: value for key, value in os.environ.items() if key != "CI"}


def write(root, tree):
    for name, text in tree.items():
        path = Path(root) / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)


def run(tree, *args, env=ENV):
    with tempfile.TemporaryDirectory() as tmp:
        write(tmp, tree)
        done = subprocess.run([sys.executable, str(SCRIPT), "--root", tmp, *args],
                              capture_output=True, text=True, env=env)
        return done.returncode, done.stdout


def changed(change, *args):
    """Runs the check on a fixture repository whose HEAD applies `change` to the clean tree."""
    with tempfile.TemporaryDirectory() as tmp:
        git = ("git", "-C", tmp, "-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false")
        subprocess.run(["git", "init", "-q", tmp], check=True)
        write(tmp, TREE)
        subprocess.run([*git, "add", "-A"], check=True)
        subprocess.run([*git, "commit", "-qm", "base"], check=True)
        write(tmp, change)
        subprocess.run([*git, "add", "-A"], check=True)
        subprocess.run([*git, "commit", "-qm", "head"], check=True)
        done = subprocess.run([sys.executable, str(SCRIPT), "--root", tmp, *args],
                              capture_output=True, text=True, env=ENV)
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
        done = subprocess.run([sys.executable, str(SCRIPT), "--diff-base", "HEAD", "--policy", "b3b4"],
                              capture_output=True, text=True, env=ENV)
        self.assertEqual((done.returncode, done.stdout), (0, "B3b-4 dormancy: PASS\n"))

    def test_a_changed_forbidden_file_fails_and_only_the_gated_declarations_pass(self):
        base = ("--diff-base", "HEAD~1")
        policy_base = (*base, "--policy")
        code, out = changed({"src/services/turn_orchestrator/incarnation.rs": "fn spawn() {}\n"},
                            *policy_base, "4a2b-0")
        self.assertEqual(code, 1, out)
        self.assertIn("incarnation.rs: changed against HEAD~1 but must stay as it is", out)
        code, out = changed({"src/services/turn_orchestrator/injected_inputs.rs": "fn f() {}\n"}, *base)
        self.assertEqual(code, 1, out)
        self.assertIn("injected_inputs.rs: changed against", out)
        # The base tree without the registry declaration; HEAD adds exactly its three gated lines.
        declared = TREE[PURGE].replace('#[cfg(test)]\n#[path = "recovery_registry/mod.rs"]\n'
                                       'pub(crate) mod recovery_registry;\n', "")
        with tempfile.TemporaryDirectory() as tmp:
            git = ("git", "-C", tmp, "-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false")
            subprocess.run(["git", "init", "-q", tmp], check=True)
            write(tmp, {**TREE, PURGE: declared})
            subprocess.run([*git, "add", "-A"], check=True)
            subprocess.run([*git, "commit", "-qm", "base"], check=True)
            for head, expected in ((TREE[PURGE], 0), (TREE[PURGE] + "fn reap() {}\n", 1)):
                write(tmp, {PURGE: head})
                subprocess.run([*git, "commit", "-qam", "head"], check=True)
                done = subprocess.run([sys.executable, str(SCRIPT), "--root", tmp, "--ci", *policy_base,
                                       "b3b4"], capture_output=True, text=True, env=ENV)
                self.assertEqual(done.returncode, expected, done.stdout)
                subprocess.run([*git, "reset", "-q", "--hard", "HEAD~1"], check=True)
        self.assertIn("`fn reap() {}` changed; only its gated declarations may", done.stdout)

    def test_ci_freezes_no_file_but_a_slice_policy_does_and_both_refuse_a_link(self):
        ci = ("--ci", "--diff-base", "HEAD~1", "--policy")
        unrelated = {"src/db/agents.rs": "fn renamed() {}\n",
                     "migrations/postgres/0200_agents_note.sql": "ALTER TABLE agents ADD COLUMN note TEXT;\n"}
        self.assertEqual(changed(unrelated, *ci, "dormant"), (0, "B3b-4 dormancy: PASS\n"))
        code, out = changed(unrelated, *ci, "4a2b-0")
        self.assertEqual(code, 1, out)
        self.assertIn("src/db/agents.rs: changed against HEAD~1 but must stay as it is", out)
        self.assertIn("0200_agents_note.sql: changed against HEAD~1", out)
        mailbox = "src/services/turn_orchestrator.rs"
        link = {mailbox: "fn boot() { registry_purge::recovery_custody::CustodyStore::new(); }\n"
                + TREE[mailbox]}
        for policy in ("dormant", "4a2b-0"):
            code, out = changed(link, *ci, policy)
            self.assertEqual(code, 1, (policy, out))
            self.assertIn("reference to a dormant module outside it", out)

    def test_ci_without_a_base_or_a_policy_is_unverified_not_pass(self):
        for args, env in (((), {**ENV, "CI": "true"}), (("--ci",), ENV),
                          (("--ci", "--diff-base", ""), ENV), (("--ci", "--diff-base", "HEAD"), ENV)):
            code, out = run(TREE, *args, env=env)
            self.assertEqual(code, 1, (args, out))
            self.assertIn("unverified: CI needs --diff-base and --policy", out)
        code, out = run(TREE, "--ci", "--diff-base", "no-such-ref", "--policy", "b3b4")
        self.assertEqual(code, 1, out)
        self.assertIn("unverified: no diff against no-such-ref", out)

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

    def test_the_actor_slice_takes_only_its_moved_body_and_gated_wiring(self):
        policy = ("--ci", "--diff-base", "HEAD~1", "--policy", "4a2b-1")
        self.assert_fails(planted(MAILBOX, VARIANT, VARIANT.replace("    #[cfg(test)]\n", "")),
                          "declaration not under #[cfg(test)]")
        moved = {MAILBOX: TREE[MAILBOX].replace(REQUEST, ""),
                 INCARNATION: "impl Handle {\n" + REQUEST.replace("async fn", "pub(super) async fn") + "}\n"}
        wired = {PURGE: TREE[PURGE] + "    #[cfg(test)]\n    M::Carried(carried) => carried.refuse_closed(),\n"}
        self.assertEqual(changed({**moved, **wired}, *policy), (0, "B3b-4 dormancy: PASS\n"))
        # A body dropped from the mailbox without reappearing in incarnation is a rewrite, not a move.
        code, out = changed({MAILBOX: moved[MAILBOX]}, *policy)
        self.assertEqual(code, 1, out)
        self.assertIn("`self.sender.send(msg)` changed; only its gated declarations may", out)
        ungated = {PURGE: wired[PURGE].replace("    #[cfg(test)]\n    M::Carried", "    M::Carried")}
        code, out = changed(ungated, *policy)
        self.assertEqual(code, 1, out)
        self.assertIn("wiring not under #[cfg(test)]: M::Carried(carried) => carried.refuse_closed(),", out)
        for writer in ("src/services/turn_orchestrator/scoped_claim.rs", "src/services/turn_orchestrator/queued_head.rs"):
            code, out = changed({writer: "fn claim() {}\n"}, *policy)
            self.assertEqual(code, 1, (writer, out))
            self.assertIn(f"{writer}: changed against HEAD~1 but must stay as it is", out)

    def test_a_migration_that_touches_existing_data_fails(self):
        path = "migrations/postgres/0140_channel_recovery_custody.sql"
        self.assert_fails({**TREE, path: TREE[path] + "ALTER TABLE o_channel_homes ADD COLUMN x INT;\n"},
                          "not additive")


if __name__ == "__main__":
    unittest.main()
