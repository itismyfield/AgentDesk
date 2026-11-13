"""Admission and mapping-only reduction contracts on committed synthetic trees."""
from copy import deepcopy
from collections import Counter
import csv
import io
import json
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from audit_maintainability.checks import module_layer_baseline as snapshots
from audit_maintainability.checks import module_layer_ratchet as ratchet
from audit_maintainability.checks.module_layers import analyze
from ci.rust_test_sources import rust_tokens


class ClassificationAdmissionTest(unittest.TestCase):
    def setUp(self):
        self.temporary = TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.put("Cargo.toml", '[package]\nname="fixture"\nversion="0.1.0"\nedition="2021"\n')
        self.put("src/lib.rs", "pub mod lower; pub mod upper; pub mod mixed; pub mod other;\n")
        self.put("src/lower.rs", "pub fn run() { crate::upper::hit(); crate::mixed::hit(); }\n")
        self.put("src/upper.rs", "pub fn hit() {}\n")
        self.put("src/mixed.rs", "pub fn hit() {}\n")
        self.put("src/other.rs", "pub fn hit() {}\n")
        self.put(snapshots.MAPPING, self.mapping())
        self.put(ratchet.MOVES, "schema_version=1\nmoves=[]\n")
        self.commit()
        self.anchor = self.git("rev-parse", "HEAD")
        self.path = self.repo / snapshots.BASELINE
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.accepted = self.measure()
        self.path.write_text(snapshots.encode_baseline(self.accepted))
        self.put(snapshots.MAPPING, self.mapping(admit=True))

    def put(self, relative, body):
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body)

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.repo), *args], text=True).strip()

    def commit(self):
        self.git("add", "Cargo.toml", "src", "scripts")
        self.git("commit", "-qm", "fixture")

    @staticmethod
    def mapping(admit=False, lower="foundation", upper="assembly"):
        rules = [("", "assembly"), ("lower", lower), ("upper", upper)]
        if admit:
            rules.append(("mixed", "adapter"))
        return ('schema_version=1\n[layers]\nfoundation=1\nadapter=2\nassembly=5\n'
                + "".join(f'[[rules]]\nprefix="{prefix}"\nlayer="{layer}"\nreason="fixture"\n'
                          for prefix, layer in rules))

    def measure(self):
        occurrences = []
        graph = analyze(self.repo, occurrence_projection=occurrences)
        return snapshots.baseline(graph, snapshots.load_mapping(self.repo),
                                  self.git("rev-parse", "HEAD"), occurrences)

    def cli(self, *extra, mode="--admit-classification", previous=None, ok=True):
        command = [sys.executable, str(ROOT / snapshots.GENERATOR.removeprefix("python3 ")),
                   "--repo", str(self.repo), "--input-sha", self.git("rev-parse", "HEAD"),
                   "--output", str(self.path), mode]
        if mode == "--admit-classification":
            command += ["--previous-mapping-rev", previous or self.anchor]
        result = subprocess.run([*command, *extra], capture_output=True, text=True)
        self.assertEqual(result.returncode == 0, ok, result.stdout + result.stderr)
        return result

    def refuse(self, *args, **kwargs):
        original = self.path.read_bytes()
        result = self.cli(*args, ok=False, **kwargs)
        self.assertEqual(self.path.read_bytes(), original)
        self.assertFalse(self.path.with_suffix(".json.lock").exists())
        return result

    def compare(self, accepted):
        return ratchet.compare(accepted, self.measure(), {"schema_version": 1, "moves": []})

    def test_exact_indeterminate_transition_keeps_existing_reverse_ceiling(self):
        old_reverse = deepcopy(self.accepted["reverse_dependencies"])
        old_indeterminate = {r["id"]: r for r in self.accepted["indeterminate_dependencies"]}
        candidate = self.measure()
        added = [r for r in candidate["reverse_dependencies"] if r not in old_reverse]
        self.assertEqual(len(added), 1)
        self.cli()
        updated = json.loads(self.path.read_bytes())
        self.assertTrue(all(row in updated["reverse_dependencies"] for row in old_reverse))
        self.assertEqual({r["id"] for r in updated["reverse_dependencies"]},
                         {r["id"] for r in old_reverse + added})
        for row in added:
            self.assertEqual(row, old_indeterminate[row["id"]])
            self.assertIn({**row, "section": "indeterminate_dependencies", "count": 0}, updated["zero_rules"])
        self.assertEqual(self.compare(updated)["errors"], [])

    def test_known_layer_change_is_refused(self):
        self.put(snapshots.MAPPING, self.mapping(admit=True, lower="adapter"))
        self.assertIn("known", self.refuse().stderr)

    def test_all_130_existing_reverse_records_and_316_occurrences_are_preserved(self):
        mapping = snapshots.load_mapping(self.repo)
        previous = deepcopy(mapping)
        previous["rules"] = [row for row in previous["rules"] if row["prefix"] != "mixed"]
        node = lambda name: "src/lib.rs::crate::" + name
        edges = [{"source": node(f"lower::leaf{i}"), "target": node("upper"),
                  "kind": "path", "sites": [f"src/lower.rs:{i + 1}"]} for i in range(130)]
        edges.append({"source": node("lower::leaf0"), "target": node("mixed"),
                      "kind": "path", "sites": ["src/lower.rs:200"]})
        graph = {"nodes": sorted({e[k] for e in edges for k in ("source", "target")}),
                 "edges": edges, "sccs": [], "coverage": {"complete": True}}
        occurrences = [{k: edge[k] for k in ("source", "target", "kind")} | {"count": count}
                       for edge, count in zip(edges, [3] * 56 + [2] * 74 + [1])]
        accepted = snapshots.baseline(graph, previous, self.anchor, occurrences)
        current = snapshots.baseline(graph, mapping, self.anchor, occurrences)
        self.assertEqual((len(accepted["reverse_dependencies"]),
                          sum(row["count"] for row in accepted["reverse_dependencies"])), (130, 316))
        admitted, _rows = snapshots._admit_classification(accepted, current,
            {"schema_version": 1, "moves": []}, graph, occurrences, mapping, previous)
        result = ratchet.reduced_baseline(admitted, current, {"schema_version": 1, "moves": []})
        self.assertEqual([row for row in result["reverse_dependencies"] if row["id"] in
                          {r["id"] for r in accepted["reverse_dependencies"]}], accepted["reverse_dependencies"])

    def test_layer_rank_change_is_refused(self):
        self.put(snapshots.MAPPING, self.mapping(admit=True).replace("foundation=1", "foundation=0"))
        self.assertIn("rank", self.refuse().stderr)

    def test_previous_mapping_hash_mismatch_is_refused(self):
        self.put(snapshots.MAPPING, self.mapping().replace('reason="fixture"', 'reason="other evidence"'))
        self.commit()
        wrong_anchor = self.git("rev-parse", "HEAD")
        self.put(snapshots.MAPPING, self.mapping(admit=True))
        self.assertIn("hash", self.refuse(previous=wrong_anchor).stderr)

    def test_source_anchor_mismatch_and_untracked_source_are_refused(self):
        self.put("src/mixed.rs", "pub fn hit() {} pub fn new() {}\n")
        self.assertIn("source tree", self.refuse().stderr)
        self.put("src/mixed.rs", "pub fn hit() {}\n")
        self.put("src/untracked.rs", "pub fn extra() {}\n")
        self.assertIn("untracked", self.refuse().stderr)

    def test_previous_projection_error_cannot_be_admitted(self):
        self.put("src/lower.rs", "pub fn run() { crate::upper::hit(); crate::upper::hit(); crate::mixed::hit(); }\n")
        self.commit()
        self.assertIn("previous projection", self.refuse().stderr)

    def test_failures_preserve_crlf_baseline_bytes_and_report(self):
        self.path.write_bytes(self.path.read_bytes().replace(b"\n", b"\r\n"))
        report = self.repo / "report.tsv"
        report.write_text("previous evidence")
        self.put(snapshots.MAPPING, self.mapping(admit=True, upper="adapter"))
        self.refuse("--report", str(report))
        self.assertEqual(report.read_text(), "previous evidence")

    def test_admitted_edge_growth_is_an_increase_error(self):
        self.cli()
        updated = json.loads(self.path.read_bytes())
        self.put("src/lower.rs", "pub fn run() { crate::upper::hit(); crate::mixed::hit(); crate::mixed::hit(); }\n")
        errors = self.compare(updated)["errors"]
        self.assertTrue(any(r["code"] == "increase" and r["section"] == "reverse_dependencies" for r in errors))

    def test_reverting_mapping_cannot_hide_admitted_debt(self):
        self.cli()
        updated = json.loads(self.path.read_bytes())
        self.put(snapshots.MAPPING, self.mapping())
        self.assertTrue(self.compare(updated)["errors"])

    def test_other_warning_growth_is_not_absorbed(self):
        self.put("src/lower.rs", "pub fn run() { crate::upper::hit(); crate::mixed::hit(); crate::other::hit(); unknown!(); }\n")
        self.put("src/new_unknown.rs", "pub fn hit() {}\n")
        self.put("src/new_unknown2.rs", "pub fn hit() {}\n")
        self.put("src/lib.rs", (self.repo / "src/lib.rs").read_text() + "pub mod new_unknown; pub mod new_unknown2;\n")
        self.commit()
        self.cli()
        updated = json.loads(self.path.read_bytes())
        self.assertEqual(updated["unclassified_modules"], self.accepted["unclassified_modules"])
        self.assertLess(updated["unclassified_modules"], self.measure()["unclassified_modules"])
        self.assertFalse(any(r["target"].endswith("::other") for r in updated["indeterminate_dependencies"]))
        self.assertEqual(updated["uncertainties"], self.accepted["uncertainties"])

    def test_same_input_replay_is_refused(self):
        self.cli()
        self.assertIn("hash", self.refuse().stderr)

    def test_mapping_only_reduction_requires_matching_reclassify_prefix(self):
        self.put(snapshots.MAPPING, self.mapping(lower="assembly"))
        for prefix in (None, "low", "other"):
            args = [] if prefix is None else ["--reclassify", prefix]
            self.assertIn("reclassify", self.refuse(*args, mode="--update-reductions").stderr)
        self.cli("--reclassify", "lower", mode="--update-reductions")
        updated = json.loads(self.path.read_bytes())
        self.assertEqual(updated["reverse_dependencies"], [])
        self.assertIn("--reclassify lower", updated["generation_command"])

    def test_actual_edge_deletion_and_occurrence_decrease_are_allowed(self):
        for source in ("pub fn run() { crate::mixed::hit(); }\n",
                       "pub fn run() { crate::upper::hit(); crate::mixed::hit(); }\n"):
            with self.subTest(source=source):
                self.put(snapshots.MAPPING, self.mapping())
                self.put("src/lower.rs", "pub fn run() { crate::upper::hit(); crate::upper::hit(); crate::mixed::hit(); }\n")
                self.commit()
                self.path.write_text(snapshots.encode_baseline(self.measure()))
                self.put("src/lower.rs", source)
                self.commit()
                self.cli(mode="--update-reductions")
                self.assertLess(sum(r["count"] for r in json.loads(self.path.read_bytes())["reverse_dependencies"]), 2)

    def test_existing_reverse_growth_and_zero_rule_revival_are_refused(self):
        for zero in (False, True):
            with self.subTest(zero=zero):
                accepted = deepcopy(self.accepted)
                row = accepted["reverse_dependencies"][0]
                if zero:
                    accepted["reverse_dependencies"] = []
                    accepted["zero_rules"].append({**row, "section": "reverse_dependencies", "count": 0})
                else:
                    self.put("src/lower.rs", "pub fn run() { crate::upper::hit(); crate::upper::hit(); crate::mixed::hit(); }\n")
                    self.commit()
                self.path.write_text(snapshots.encode_baseline(accepted))
                self.refuse()

    def test_report_records_absent_accepted_count_and_growth(self):
        self.put("src/other.rs", "pub fn hit() { crate::mixed::hit(); crate::mixed::hit(); }\n")
        mapping = self.mapping(admit=True) + '[[rules]]\nprefix="other"\nlayer="foundation"\nreason="fixture"\n'
        self.put(snapshots.MAPPING, mapping)
        self.commit()
        report = self.repo / "report.tsv"
        self.cli("--report", str(report))
        rows = list(csv.DictReader(io.StringIO(report.read_text()), delimiter="\t"))
        added = next(row for row in rows if row["source"].endswith("::other"))
        self.assertEqual(int(added["accepted_indeterminate_count"]), 0)
        self.assertEqual(int(added["grown_since_accepted"]), 2)
        self.assertEqual(sum(int(r["accepted_indeterminate_count"]) for r in rows), 1)
        self.assertEqual(sum(int(r["grown_since_accepted"]) for r in rows), 2)

    def test_dry_run_is_reviewable_without_changing_baseline(self):
        original = self.path.read_bytes()
        report = self.repo / "report.tsv"
        result = self.cli("--dry-run", "--report", str(report))
        self.assertEqual(self.path.read_bytes(), original)
        projected = json.loads(result.stdout)
        self.assertEqual(len(projected["reverse_dependencies"]), 2)
        self.cli()
        self.assertEqual(result.stdout.encode(), self.path.read_bytes())

    def test_empty_admission_matches_ordinary_reduction_bytes(self):
        self.put(snapshots.MAPPING, self.mapping() + '[[rules]]\nprefix="other"\nlayer="adapter"\nreason="fixture"\n')
        admitted = self.cli("--dry-run").stdout
        reduced = self.cli("--dry-run", mode="--update-reductions").stdout
        self.assertEqual(admitted, reduced)

    def test_movement_conflict_and_report_over_baseline_are_refused(self):
        candidate = self.measure()
        added = next(r for r in candidate["reverse_dependencies"] if r not in self.accepted["reverse_dependencies"])
        old = self.accepted["reverse_dependencies"][0]
        self.put(ratchet.MOVES, ('schema_version=1\n[[moves]]\nsection="reverse_dependencies"\n'
                                f'old_id="{old["id"]}"\nnew_id="{added["id"]}"\n'))
        self.refuse()
        self.put(ratchet.MOVES, "schema_version=1\nmoves=[]\n")
        self.refuse("--report", str(self.path))

    def test_report_hardlink_cannot_truncate_accepted_bytes(self):
        report = self.repo / "alias.tsv"
        report.hardlink_to(self.path)
        self.refuse("--report", str(report))
        self.assertEqual(report.read_bytes(), self.path.read_bytes())


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
                    codes = {"E0425"}
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
