"""Replay and recovery evidence privacy contracts for the observer-scoped authority types."""
from collections import Counter
import json
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from ci.rust_test_sources import rust_tokens


class ReplayEvidencePrivacyTest(unittest.TestCase):
    def setUp(self):
        self.temporary = TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.scratch = Path(self.temporary.name)
        self.paths = {
            "module": ROOT / "src/domain/replay_evidence.rs",
            "proof": ROOT / "src/domain/replay_evidence/live.rs",
            "issuer": ROOT / "src/services/replay_evidence/live_issuer.rs",
            "key": ROOT / "src/services/discord/replay_policy/live/issuer_key.rs",
        }
        for path in self.paths.values():
            self.assertTrue(path.is_file(), f"missing replay authority source: {path}")

    def compile(self, body="", *, test=False):
        module, key = (json.dumps(str(self.paths[name])) for name in ("module", "key"))
        recovery_key = json.dumps(str(ROOT / "src/services/discord/replay_policy/recovery/issuer_key.rs"))
        root_key = json.dumps(str(ROOT / "src/cli/dcserver/replay_recovery/issuer_key.rs"))
        source = f"""
mod domain {{ #[path = {module}] pub(crate) mod replay_evidence; }}
mod cli {{ pub(crate) mod dcserver {{ pub(crate) mod replay_recovery {{
    #[path = {root_key}] pub(crate) mod issuer_key;
}} }} }}
mod services {{ pub(crate) mod discord {{ pub(crate) mod replay_policy {{
    pub(crate) mod recovery {{ #[path = {recovery_key}] pub(crate) mod issuer_key; }}
    pub(crate) mod live {{
        #[path = {key}] pub(crate) mod issuer_key;
        pub(crate) fn owned() -> (
            crate::domain::replay_evidence::live::TerminalEvidence,
            crate::domain::replay_evidence::live::NoEffectEvidence,
        ) {{
            use crate::domain::replay_evidence::{{live::issuer, TerminalKind}};
            (
                issuer::terminal(issuer_key::ObservationKey(()), 7, "nonce".into(), TerminalKind::Error),
                issuer::no_effect(issuer_key::ObservationKey(()), 7, "nonce".into()),
            )
        }}
    }}
}} }} }}
use domain::replay_evidence::live::{{TerminalEvidence, NoEffectEvidence}};
use services::discord::replay_policy::live::issuer_key::ObservationKey;
fn positive_control() {{
    let (terminal, quiet) = services::discord::replay_policy::live::owned();
    assert!(terminal.names(7, "nonce"));
    assert!(quiet.names(7, "nonce"));
    assert_eq!(terminal.kind(), domain::replay_evidence::TerminalKind::Error);
}}
{body}
"""
        path = self.scratch / "privacy.rs"
        path.write_text(source)
        command = ["rustc", "--edition=2024", "--crate-type=lib", "--emit=metadata",
                   "--error-format=json", str(path), "--out-dir", str(self.scratch)]
        if test:
            command += ["--cfg", "test"]
        return subprocess.run(command, text=True, capture_output=True, timeout=30)

    def assert_compiles(self, **kwargs):
        result = self.compile(**kwargs)
        self.assertEqual(result.returncode, 0, result.stderr)

    def assert_refused(self, body, codes, **kwargs):
        result = self.compile(body, **kwargs)
        diagnostics = [json.loads(line) for line in result.stderr.splitlines() if line.startswith("{")]
        errors = [d for d in diagnostics if d.get("level") == "error" and d.get("code")]
        self.assertNotEqual(result.returncode, 0, "sibling unexpectedly acquired replay authority")
        self.assertTrue(errors, result.stderr)
        self.assertTrue(all(d["code"]["code"] in codes for d in errors), result.stderr)

    def assert_sealed_layout(self, values, *, fields=False, key=None, modules=()):
        self.assertEqual([values[i + 1] for i, value in enumerate(values) if value == "mod"], list(modules))
        if key:
            self.assertEqual(values, ["pub", "(", "crate", ")", "struct", key, "(",
                                      "pub", "(", "super", ")", "(", ")", ")", ";"])
        if fields:
            for i, value in enumerate(values):
                if value != "struct":
                    continue
                start = values.index("{", i)
                end, depth = start + 1, 1
                while depth:
                    depth += (values[end] == "{") - (values[end] == "}")
                    end += 1
                self.assertNotIn("pub", values[start + 1:end - 1], "evidence fields stay private to their issuing subtree")

    def test_real_issuer_compiles_only_in_the_original_observer_scope(self):
        self.assert_compiles()
        self.assert_compiles(test=True)

    def test_sibling_cannot_construct_replay_proofs_or_the_observer_key(self):
        self.assert_compiles()
        cases = {
            "terminal": 'fn sibling() { let _ = TerminalEvidence { receipt_id: 7, nonce: "n".into(), kind: domain::replay_evidence::TerminalKind::Error }; }',
            "no_effect": 'fn sibling() { let _ = NoEffectEvidence { receipt_id: 7, nonce: "n".into() }; }',
            "key": 'fn sibling() { let _ = ObservationKey(()); }',
            "terminal_receipt": 'fn sibling(mut value: TerminalEvidence) { value.receipt_id = 8; }',
            "terminal_nonce": 'fn sibling(mut value: TerminalEvidence) { value.nonce = "other".into(); }',
            "terminal_kind": 'fn sibling(mut value: TerminalEvidence) { value.kind = domain::replay_evidence::TerminalKind::Done; }',
            "quiet_receipt": 'fn sibling(mut value: NoEffectEvidence) { value.receipt_id = 8; }',
            "quiet_nonce": 'fn sibling(mut value: NoEffectEvidence) { value.nonce = "other".into(); }',
            "wrong_terminal_key": 'fn sibling() { let _ = domain::replay_evidence::live::issuer::terminal((), 7, "n".into(), domain::replay_evidence::TerminalKind::Error); }',
            "wrong_quiet_key": 'fn sibling() { let _ = domain::replay_evidence::live::issuer::no_effect(false, 7, "n".into()); }',
        }
        for name, body in cases.items():
            with self.subTest(name=name):
                codes = {"E0308"} if name.startswith("wrong_") else {"E0451", "E0423", "E0616"}
                self.assert_refused(body, codes)

    def test_replay_authority_has_no_default_clone_or_copy_escape(self):
        self.assert_compiles()
        for ty in ("TerminalEvidence", "NoEffectEvidence", "ObservationKey"):
            bodies = {
                "default": f"fn sibling() {{ let _: {ty} = Default::default(); }}",
                "clone": f"fn sibling(value: &{ty}) -> {ty} {{ Clone::clone(value) }}",
                "copy": f"fn need_copy<T: Copy>() {{}} fn sibling() {{ need_copy::<{ty}>(); }}",
            }
            for trait, body in bodies.items():
                with self.subTest(type=ty, trait=trait):
                    self.assert_refused(body, {"E0277"})
                    self.assert_refused(body, {"E0277"}, test=True)

    def test_authority_api_has_no_extra_factory_or_test_constructor(self):
        expected = {"module": Counter(), "proof": Counter(names=2, kind=1),
                    "issuer": Counter(terminal=1, no_effect=1), "key": Counter()}
        for name, path in self.paths.items():
            # Keep cfg(test) items: test-only constructors can otherwise bypass the same gate.
            tokens = rust_tokens(path.read_text(), extended=True)
            values = [token.value for token in tokens if token.kind != "string"]
            self.assert_sealed_layout(values, fields=name == "proof",
                                      key="ObservationKey" if name == "key" else None,
                                      modules={"module": ("live", "recovery"), "proof": ("issuer",)}.get(name, ()))
            public = Counter()
            for i, value in enumerate(values):
                if value == "fn":
                    if name == "issuer":
                        self.assertEqual(values[i + 2], "(", "issuer must require the concrete observer key")
                        first_end = values.index(",", i + 3)
                        self.assertEqual(values[i + 4:first_end], [":", "ObservationKey"])
                    start = i - 1
                    while start >= 0 and values[start] not in {"{", "}", ";"}:
                        start -= 1
                    if "pub" in values[start + 1:i]:
                        public[values[i + 1]] += 1
                if value == "impl":
                    end = values.index("{", i)
                    self.assertNotIn("for", values[i:end], f"trait factory/replication impl in {path}")
            self.assertEqual(public, expected[name], f"unreviewed authority factory in {path}")
            if name != "module":
                for i, value in enumerate(values):
                    if value == "derive":
                        end = values.index(")", i)
                        forbidden = {"Default", "Clone", "Copy", "Serialize", "Deserialize"}
                        self.assertFalse(forbidden.intersection(values[i:end]), f"authority derive in {path}")
            self.assertNotIn("macro_rules", values, f"generated authority API in {path}")
            self.assertNotIn("!", values, f"macro-generated authority API in {path}")


class RecoveryEvidencePrivacyTest(ReplayEvidencePrivacyTest):
    def setUp(self):
        super().setUp()
        self.paths = {
            "module": ROOT / "src/domain/replay_evidence.rs",
            "recovery": ROOT / "src/domain/replay_evidence/recovery.rs",
            "snapshot": ROOT / "src/domain/replay_evidence/recovery/snapshot.rs",
            "root": ROOT / "src/domain/replay_evidence/recovery/root.rs",
            "issuer": ROOT / "src/services/replay_evidence/recovery_issuer.rs",
            "root_issuer": ROOT / "src/cli/dcserver/replay_root_issuer.rs",
            "key": ROOT / "src/services/discord/replay_policy/recovery/issuer_key.rs",
            "root_key": ROOT / "src/cli/dcserver/replay_recovery/issuer_key.rs",
        }
        for path in self.paths.values():
            self.assertTrue(path.is_file(), f"missing recovery authority source: {path}")

    def compile(self, body="", *, test=False):
        paths = {name: json.dumps(str(path)) for name, path in self.paths.items()}
        live_key = json.dumps(str(ROOT / "src/services/discord/replay_policy/live/issuer_key.rs"))
        source = f"""
mod domain {{ #[path = {paths['module']}] pub(crate) mod replay_evidence; }}
mod cli {{ pub(crate) mod dcserver {{ pub(crate) mod replay_recovery {{
    #[path = {paths['root_key']}] pub(crate) mod issuer_key;
    #[cfg(unix)]
    pub(crate) fn owned(path: std::path::PathBuf, lock: std::sync::Arc<std::fs::File>) ->
        crate::domain::replay_evidence::recovery::root::OwnedRuntimeRoot {{
        crate::domain::replay_evidence::recovery::root::issuer::owned(
            issuer_key::SingletonRootKey(()), path, lock)
    }}
}} }} }}
mod services {{ pub(crate) mod discord {{ pub(crate) mod replay_policy {{
    pub(crate) mod live {{ #[path = {live_key}] pub(crate) mod issuer_key; }}
    pub(crate) mod recovery {{
        #[path = {paths['key']}] pub(crate) mod issuer_key;
        pub(crate) fn observed(root: crate::domain::replay_evidence::recovery::root::OwnedRuntimeRoot)
            -> crate::domain::replay_evidence::recovery::LocalSnapshot {{
            use crate::domain::replay_evidence::{{SnapshotIdentity, recovery::snapshot::issuer}};
            issuer::loaded(issuer_key::LoadedSnapshotKey(()), root,
                SnapshotIdentity {{ replay_receipt_id: Some(7), provider: "codex", channel_id: 2,
                    user_msg_id: 3, request_owner_user_id: 4, user_text: " raw ",
                    source_message_ids: &[3, 3, 1], turn_nonce: Some("turn") }},
                (1, 1, false, false), "instance".into(), "incarnation")
        }}
    }}
}} }} }}
use domain::replay_evidence::recovery::{{LocalSnapshot, root::OwnedRuntimeRoot}};
use services::discord::replay_policy::recovery::issuer_key::LoadedSnapshotKey;
use cli::dcserver::replay_recovery::issuer_key::SingletonRootKey;
fn positive_control(root: OwnedRuntimeRoot) {{
    let proof = services::discord::replay_policy::recovery::observed(root.clone());
    assert!(proof.understood());
    assert_eq!(proof.state().user_text, " raw ");
    assert_eq!(proof.instance(), "instance");
    assert_eq!(proof.incarnation(), "incarnation");
}}
#[cfg(unix)]
fn singleton_positive_control(path: std::path::PathBuf, lock: std::sync::Arc<std::fs::File>) {{
    positive_control(cli::dcserver::replay_recovery::owned(path, lock));
}}
{body}
"""
        path = self.scratch / "recovery_privacy.rs"
        path.write_text(source)
        command = ["rustc", "--edition=2024", "--crate-type=lib", "--emit=metadata",
                   "--error-format=json", str(path), "--out-dir", str(self.scratch)]
        if test:
            command += ["--cfg", "test"]
        return subprocess.run(command, text=True, capture_output=True, timeout=30)

    def test_real_issuer_compiles_only_in_the_original_observer_scope(self):
        self.assert_compiles()
        self.assert_compiles(test=True)

    def test_sibling_cannot_construct_replay_proofs_or_the_observer_key(self):
        self.assert_compiles()
        cases = {
            "snapshot_update": 'fn sibling(proof: LocalSnapshot) { let _ = LocalSnapshot { ..proof }; }',
            "snapshot_root": 'fn sibling(mut proof: LocalSnapshot, root: OwnedRuntimeRoot) { proof._root = root; }',
            "snapshot_format": 'fn sibling(mut proof: LocalSnapshot) { proof.format = (1, 1, false, false); }',
            "snapshot_instance": 'fn sibling(mut proof: LocalSnapshot) { proof.instance = "other".into(); }',
            "snapshot_incarnation": 'fn sibling(mut proof: LocalSnapshot) { proof.incarnation = "other".into(); }',
            "root": 'fn sibling(path: std::path::PathBuf, lock: std::sync::Arc<std::fs::File>) { let _ = OwnedRuntimeRoot { root: path, _lock: lock }; }',
            "root_path": 'fn sibling(mut root: OwnedRuntimeRoot) { root.root = "/other".into(); }',
            "root_lock": 'fn sibling(mut root: OwnedRuntimeRoot, lock: std::sync::Arc<std::fs::File>) { root._lock = lock; }',
            "loader_key": 'fn sibling() { let _ = LoadedSnapshotKey(()); }',
            "root_key": 'fn sibling() { let _ = SingletonRootKey(()); }',
            "wrong_root_key": 'fn sibling(path: std::path::PathBuf, lock: std::sync::Arc<std::fs::File>) { let _ = domain::replay_evidence::recovery::root::issuer::owned((), path, lock); }',
            "wrong_loaded_key": 'fn sibling(root: OwnedRuntimeRoot, proof: &LocalSnapshot) { let _ = domain::replay_evidence::recovery::snapshot::issuer::loaded(false, root, proof.state(), (1, 1, false, false), "node".into(), "owner"); }',
        }
        for name, body in cases.items():
            with self.subTest(name=name):
                codes = {"E0308"} if name.startswith("wrong_") else {"E0451", "E0423", "E0616"}
                if name == "wrong_root_key" and sys.platform == "win32":
                    codes = {"E0433"}
                self.assert_refused(body, codes)

    def test_replay_authority_has_no_default_clone_or_copy_escape(self):
        self.assert_compiles()
        for ty in ("LocalSnapshot", "OwnedRuntimeRoot", "LoadedSnapshotKey", "SingletonRootKey"):
            bodies = {
                "default": f"fn sibling() {{ let _: {ty} = Default::default(); }}",
                "copy": f"fn need_copy<T: Copy>() {{}} fn sibling() {{ need_copy::<{ty}>(); }}",
            }
            if ty != "OwnedRuntimeRoot":
                bodies["clone"] = f"fn sibling(value: &{ty}) -> {ty} {{ Clone::clone(value) }}"
            for trait, body in bodies.items():
                with self.subTest(type=ty, trait=trait):
                    self.assert_refused(body, {"E0277"})
                    self.assert_refused(body, {"E0277"}, test=True)

    def test_authority_api_has_no_extra_factory_or_test_constructor(self):
        expected = {"module": Counter(), "recovery": Counter(),
                    "snapshot": Counter(state=1, understood=1, instance=1, incarnation=1),
                    "root": Counter(covers=1), "issuer": Counter(loaded=1),
                    "root_issuer": Counter(owned=1), "key": Counter(), "root_key": Counter()}
        keys = {"issuer": "LoadedSnapshotKey", "root_issuer": "SingletonRootKey"}
        for name, path in self.paths.items():
            tokens = rust_tokens(path.read_text(), extended=True)
            values = [token.value for token in tokens if token.kind != "string"]
            self.assert_sealed_layout(values, fields=name in {"snapshot", "root"},
                                      key={"key": "LoadedSnapshotKey", "root_key": "SingletonRootKey"}.get(name),
                                      modules={"module": ("live", "recovery"), "recovery": ("root", "snapshot"),
                                               "snapshot": ("issuer",), "root": ("issuer",)}.get(name, ()))
            public = Counter()
            for i, value in enumerate(values):
                if value == "fn":
                    if name in keys:
                        self.assertEqual(values[i + 2], "(", "issuer requires its concrete private key")
                        first_end = values.index(",", i + 3)
                        self.assertEqual(values[i + 4:first_end], [":", keys[name]])
                    start = i - 1
                    while start >= 0 and values[start] not in {"{", "}", ";"}:
                        start -= 1
                    if "pub" in values[start + 1:i]:
                        public[values[i + 1]] += 1
                if value == "impl":
                    end = values.index("{", i)
                    self.assertNotIn("for", values[i:end], f"trait factory/replication impl in {path}")
            self.assertEqual(public, expected[name], f"unreviewed recovery authority factory in {path}")
            if name not in {"module", "recovery"}:
                for i, value in enumerate(values):
                    if value == "derive":
                        end = values.index(")", i)
                        allowed = {"Debug", "Clone"} if name == "root" else {"Debug"}
                        derives = {v for v in values[i + 2:end] if v != ","}
                        self.assertLessEqual(derives, allowed, f"authority derive in {path}")
            self.assertNotIn("macro_rules", values, f"generated authority API in {path}")
            self.assertNotIn("const", values, f"constant authority factory in {path}")
            self.assertNotIn("static", values, f"static authority factory in {path}")
            for i, value in enumerate(values[:-1]):
                if value == "!" and values[i + 1] in {"(", "{", "["} and i > 0:
                    self.assertFalse(values[i - 1].isidentifier(), f"macro-generated authority API in {path}")


if __name__ == "__main__":
    unittest.main()
