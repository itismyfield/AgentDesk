"""Admission behavior for non-live isolated instance declarations."""

import copy
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest
import uuid

from scripts.dev_instance.files import InstanceError
from scripts.dev_instance.manifest import child_environment, prepare, read_json, validate
from scripts.dev_instance.profile import Unevaluable, require_preconditions


REPOSITORY = Path(__file__).resolve().parents[1]
SENTINEL = "FAKE_SENTINEL_NEVER_FORWARD_DISCORD_TOKEN"


def profile():
    return {
        "schema_version": 1, "name": "tui-live",
        "bots": {"claude": "900001", "codex": "900002", "driver": "900003"},
        "channels": ["900004", "900005"],
        "features": {
            "env": {
                "ADK_BUSY_INJECT": "all",
                "ADK_BUSY_INJECT_DISCORD_CHANNELS": ["900004", "900005"],
                "ADK_BUSY_INJECT_STASH_CHANNELS": [],
                "ADK_QUEUED_HEAD_INJECT": "1", "ADK_TURN_PRESENCE_TAIL": "1",
                "ADK_INTAKE_ROUTING_MODE": "enforce", "ADK_IMSG_OBLIGATIONS": "0",
            },
            "tui_writer": True, "tui_turn": True,
            "capacity_aware": True, "preferred_node": "local",
        },
    }


class InstanceDeclarationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="dev-instance-")
        self.addCleanup(self.temporary.cleanup)
        self.home = Path(self.temporary.name).resolve()
        self.base = self.home / ".adk/dev/instances"
        self.base.mkdir(parents=True)
        self.worktree = self.home / "worktree"
        self.worktree.mkdir()
        self.git("init", "-q")
        self.git("config", "user.name", "Test Fixture")
        self.git("config", "user.email", "test@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        (self.worktree / "fixture").write_text("committed fixture\n")
        self.git("add", "fixture")
        self.git("commit", "-qm", "fixture")
        self.instance_id = str(uuid.uuid4())

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.worktree), *args],
            env={"PATH": "/usr/bin:/bin", "HOME": str(self.home), "LC_ALL": "C"},
            stderr=subprocess.DEVNULL, text=True).strip()

    def prepare(self, selected=None, ambient=None, **ports):
        return prepare(self.base, self.instance_id, self.worktree,
                       selected if selected is not None else profile(),
                       ambient if ambient is not None else {}, **ports)

    def cli(self, *args):
        return subprocess.run(
            [sys.executable, "-E", "-s", "-m", "scripts.dev_instance", *map(str, args)],
            cwd=REPOSITORY, env={"PATH": "/usr/bin:/bin", "HOME": str(self.home)},
            text=True, capture_output=True, check=False)

    def witness(self, manifest):
        return {
            "instance_id": manifest["instance_id"], "head": manifest["head"],
            "applied_env": copy.deepcopy(manifest["applied_env"]),
            "features": copy.deepcopy(manifest["profile"]["features"]),
            "host": "local", "execution_node": manifest["execution_node_id"],
        }

    def test_declaration_records_private_local_targets_and_explicit_features(self):
        manifest = self.prepare()
        root = self.base / self.instance_id
        config = read_json(root / "config/agentdesk.yaml")
        receipt = read_json(root / "receipt.json")
        self.assertEqual(validate(self.base, self.instance_id, {}), manifest)
        self.assertFalse(manifest["live_ready"])
        self.assertEqual(manifest["status"], "DECLARED")
        self.assertEqual(manifest["root"], str(root))
        self.assertEqual(manifest["head"], self.git("rev-parse", "HEAD"))
        self.assertEqual(config["server"], {
            "host": "127.0.0.1", "port": manifest["api"]["port"],
            "allow_insecure_nonloopback_bind": False,
        })
        self.assertEqual(manifest["api"]["self_url"], f'http://127.0.0.1:{config["server"]["port"]}')
        pg = manifest["postgres"]
        self.assertEqual(pg["host"], "127.0.0.1")
        self.assertIn(pg["port"], range(25432, 25448))
        self.assertEqual(pg["data_dir"], str(root / "postgres/data"))
        self.assertEqual(pg["socket_dir"], str(root / "postgres/socket"))
        self.assertEqual(pg["role"], "adktest_" + self.instance_id.replace("-", ""))
        self.assertEqual(pg["database"], pg["role"])
        self.assertFalse(pg["cluster_started"])
        self.assertTrue(config["database"]["enabled"])
        self.assertEqual(config["database"]["user"], pg["role"])
        self.assertEqual(config["database"]["port"], pg["port"])
        self.assertEqual(config["tui_o"]["writer"]["channels"], [900004, 900005])
        self.assertEqual(config["tui_o"]["turn"]["channels"], [900004, 900005])
        self.assertFalse(config["tui_o"]["writer"]["all_tui"])
        self.assertFalse(config["tui_o"]["turn"]["all_owned"])
        self.assertTrue(config["cluster"]["intake_routing"]["capacity_aware"])
        self.assertTrue(manifest["preferred_node_seed_required"])
        self.assertEqual(set(config["discord"]["bots"]), {"claude", "codex"})
        self.assertEqual(receipt["applied_env"]["ADK_BUSY_INJECT_DISCORD_CHANNELS"], "900004,900005")
        self.assertEqual(receipt["features"], profile()["features"])
        self.assertNotIn(SENTINEL, json.dumps(receipt))
        for agent in config["agents"]:
            binding = agent["channels"][agent["provider"]]
            for key in ("workspace", "prompt_file"):
                self.assertTrue(Path(binding[key]).is_relative_to(root))
                self.assertTrue(Path(binding[key]).exists())

    def test_execution_node_identity_matches_config_and_preferred_node(self):
        manifest = self.prepare()
        root = self.base / self.instance_id
        receipt = read_json(root / "receipt.json")
        config = read_json(root / "config/agentdesk.yaml")
        self.assertEqual(receipt["execution_node_id"], self.instance_id)
        self.assertEqual(config["cluster"]["instance_id"], self.instance_id)
        self.assertEqual(config["cluster"]["gateway_preferred_instance_id"], self.instance_id)
        self.assertEqual(validate(self.base, self.instance_id, {}), manifest)
        witness = self.witness(manifest)
        witness["execution_node"] = self.instance_id
        require_preconditions(manifest, witness)

    def test_missing_feature_flags_are_unevaluable_before_creating_root(self):
        selected = profile()
        del selected["features"]
        with self.assertRaises(Unevaluable):
            self.prepare(selected)
        for key in list(profile()["features"]["env"]):
            with self.subTest(key=key):
                selected = profile()
                del selected["features"]["env"][key]
                with self.assertRaises(Unevaluable):
                    self.prepare(selected)
                self.assertFalse((self.base / self.instance_id).exists())
        profile_file = self.home / "profile.json"
        profile_file.write_text(json.dumps(selected))
        result = self.cli("prepare", self.worktree, "--profile", profile_file)
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertEqual(json.loads(result.stdout)["status"], "UNEVALUABLE(precondition)")

    def test_profiles_reject_inline_secrets_external_overrides_and_channel_wildcards(self):
        mutations = [
            lambda p: p.update(token=SENTINEL),
            lambda p: p.update(schema_version=True),
            lambda p: p.update(root=str(self.home / "outside")),
            lambda p: p.update(database={"url": "postgresql://prod@127.0.0.1:15432/agentdesk"}),
            lambda p: p.update(env_file=str(self.home / "launchd.env")),
            lambda p: p["features"]["env"].update({"ADK_*": "1"}),
            lambda p: p["features"]["env"].update({"DISCORD_TOKEN": SENTINEL}),
            lambda p: p["features"]["env"].update(ADK_BUSY_INJECT=SENTINEL),
            lambda p: p["features"]["env"].update(ADK_BUSY_INJECT_DISCORD_CHANNELS=["*"]),
            lambda p: p["features"]["env"].update(ADK_BUSY_INJECT_STASH_CHANNELS=["900099"]),
        ]
        for index, mutate in enumerate(mutations):
            with self.subTest(index=index):
                selected = profile()
                mutate(selected)
                with self.assertRaises(InstanceError) as caught:
                    self.prepare(selected)
                self.assertNotIn(SENTINEL, str(caught.exception))
                self.assertFalse((self.base / self.instance_id).exists())

    def test_database_environment_is_rejected_even_when_empty_before_writes(self):
        for key in ("DATABASE_URL", "PGHOST", "PGPORT", "PGSERVICE", "PGPASSFILE", "PGUNKNOWN"):
            for value in ("", "postgresql://prod@127.0.0.1:5432/agentdesk"):
                with self.subTest(key=key, empty=not value):
                    with self.assertRaisesRegex(InstanceError, "inherited database"):
                        self.prepare(ambient={key: value})
                    self.assertFalse((self.base / self.instance_id).exists())

    def test_external_and_release_ports_are_rejected(self):
        for ports in ({"api_port": 8791}, {"api_port": 18790}, {"api_port": 0}, {"api_port": 18807},
                      {"pg_port": 5432}, {"pg_port": 15432}, {"pg_port": 25431}, {"pg_port": 25448}):
            with self.subTest(ports=ports):
                with self.assertRaisesRegex(InstanceError, "test pool"):
                    self.prepare(**ports)
                self.assertFalse((self.base / self.instance_id).exists())

    def test_occupied_loopback_ports_are_rejected(self):
        for field, pool in (("api_port", range(18791, 18807)), ("pg_port", range(25432, 25448))):
            with self.subTest(field=field):
                for port in pool:
                    listener = socket.socket()
                    try:
                        listener.bind(("127.0.0.1", port))
                    except OSError:
                        listener.close()
                        continue
                    break
                else:
                    self.fail("test port pool unavailable; cannot prove occupied-port rejection")
                with listener:
                    listener.listen()
                    with self.assertRaisesRegex(InstanceError, "occupied"):
                        self.prepare(**{field: port})
                self.assertFalse((self.base / self.instance_id).exists())

    def test_child_process_receives_no_ambient_credentials_or_wildcard_environment(self):
        manifest = self.prepare()
        ambient = {key: SENTINEL for key in (
            "DISCORD_TOKEN", "CLAUDE_BOT_TOKEN", "CODEX_BOT_TOKEN", "DRIVER_BOT_TOKEN",
            "AGENTDESK_TOKEN", "OP_SERVICE_ACCOUNT_TOKEN", "MEMENTO_ACCESS_KEY",
            "ADK_UNKNOWN", "ADK_OBSERVABILITY_EVENTS_PATH", "PATH", "TMUX", "TMUX_PANE",
        )}
        environment = child_environment(manifest, ambient)
        child = subprocess.run(
            [sys.executable, "-c", "import json,os,sys; print(json.dumps([dict(os.environ),sys.argv]))"],
            env=environment, capture_output=True, text=True, check=True)
        self.assertNotIn(SENTINEL, child.stdout + child.stderr)
        observed, argv = json.loads(child.stdout)
        for key in ambient.keys() - {"PATH"}:
            self.assertNotIn(key, observed)
        self.assertEqual(observed["AGENTDESK_API_URL"], manifest["api"]["self_url"])
        self.assertEqual(observed["ADK_API_URL"], observed["AGENTDESK_API_URL"])
        self.assertEqual(observed["ADK_BUSY_INJECT"], "all")
        self.assertEqual(argv, ["-c"])
        for key in ("DATABASE_URL", "PGHOST"):
            with self.assertRaisesRegex(InstanceError, "inherited database"):
                child_environment(manifest, {key: ""})

    def test_manifest_config_and_receipt_tampering_cannot_change_targets(self):
        self.prepare()
        root = self.base / self.instance_id
        variants = (
            ("manifest.json", lambda d: d.update(token=SENTINEL)),
            ("manifest.json", lambda d: d.update(root=str(self.home / "outside"))),
            ("manifest.json", lambda d: d["postgres"].update(data_dir=str(self.home / "outside"))),
            ("manifest.json", lambda d: d["postgres"].update(dsn="postgresql://prod@127.0.0.1:15432/agentdesk")),
            ("manifest.json", lambda d: d["api"].update(host="192.0.2.1")),
            ("config/agentdesk.yaml", lambda d: d["database"].update(port=5432)),
            ("config/agentdesk.yaml", lambda d: d["server"].update(allow_insecure_nonloopback_bind=0)),
            ("config/agentdesk.yaml", lambda d: d["data"].update(dir=str(self.home / "outside"))),
            ("config/agentdesk.yaml", lambda d: d["discord"]["bots"]["claude"].update(token=SENTINEL)),
            ("receipt.json", lambda d: d.update(head="0" * 40)),
        )
        for relative, mutate in variants:
            with self.subTest(file=relative, mutate=variants.index((relative, mutate))):
                path = root / relative
                original = path.read_text()
                declaration = json.loads(original)
                mutate(declaration)
                path.write_text(json.dumps(declaration))
                try:
                    with self.assertRaises(InstanceError) as caught:
                        validate(self.base, self.instance_id, {})
                    self.assertNotIn(SENTINEL, str(caught.exception))
                finally:
                    path.write_text(original)

    def test_missing_explicit_config_has_no_sibling_fallback(self):
        self.prepare()
        root = self.base / self.instance_id
        config = root / "config/agentdesk.yaml"
        config.rename(root / "config/agentdesk.yml")
        with self.assertRaisesRegex(InstanceError, "JSON declaration"):
            validate(self.base, self.instance_id, {})

    def test_preconditions_require_observed_matching_host_node_flags_and_enabled_gates(self):
        manifest = self.prepare()
        self.assertIsNone(require_preconditions(manifest, self.witness(manifest)))
        variants = [None, {}, self.witness(manifest)]
        variants[-1]["host"] = "foreign"
        for witness in variants:
            with self.subTest(witness=witness):
                with self.assertRaises(Unevaluable):
                    require_preconditions(manifest, witness)
        for key in ("instance_id", "head", "applied_env", "features", "execution_node"):
            witness = self.witness(manifest)
            del witness[key]
            with self.subTest(missing=key), self.assertRaises(Unevaluable):
                require_preconditions(manifest, witness)
        for key in ("tui_writer", "tui_turn", "capacity_aware"):
            disabled = copy.deepcopy(manifest)
            disabled["profile"]["features"][key] = False
            with self.subTest(disabled=key), self.assertRaises(Unevaluable):
                require_preconditions(disabled, self.witness(disabled))
        for key, value in (("ADK_BUSY_INJECT", "off"), ("ADK_INTAKE_ROUTING_MODE", "observe"),
                           ("ADK_QUEUED_HEAD_INJECT", "0"), ("ADK_TURN_PRESENCE_TAIL", "0"),
                           ("ADK_BUSY_INJECT_DISCORD_CHANNELS", "")):
            disabled = copy.deepcopy(manifest)
            disabled["applied_env"][key] = value
            with self.subTest(disabled=key), self.assertRaises(Unevaluable):
                require_preconditions(disabled, self.witness(disabled))
        witness_file = self.home / "witness.json"
        witness_file.write_text("{}")
        rejected = self.cli("validate", "--instance", self.instance_id, "--witness", witness_file)
        self.assertEqual(rejected.returncode, 3, rejected.stderr)
        self.assertEqual(json.loads(rejected.stdout)["status"], "UNEVALUABLE(precondition)")
        witness_file.write_text(json.dumps(self.witness(manifest)))
        accepted = self.cli("validate", "--instance", self.instance_id, "--witness", witness_file)
        self.assertEqual(accepted.returncode, 0, accepted.stderr)
        self.assertFalse(json.loads(accepted.stdout)["live_ready"])

    def test_cli_rejects_inline_token_without_echoing_argv_or_input(self):
        for args in (("validate", "--instance", self.instance_id, "--token", SENTINEL),
                     ("prepare", self.worktree, "--profile", SENTINEL, "--root", "/outside")):
            with self.subTest(args=args):
                result = self.cli(*args)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(json.loads(result.stdout)["status"], "REJECTED")
                self.assertNotIn(SENTINEL, result.stdout + result.stderr)

    def test_dirty_worktree_or_changed_head_is_rejected(self):
        (self.worktree / "fixture").write_text("dirty\n")
        with self.assertRaisesRegex(InstanceError, "clean committed"):
            self.prepare()
        self.assertFalse((self.base / self.instance_id).exists())
        self.git("add", "fixture")
        self.git("commit", "-qm", "next fixture")
        self.prepare()
        (self.worktree / "fixture").write_text("next head\n")
        self.git("add", "fixture")
        self.git("commit", "-qm", "changed head")
        with self.assertRaisesRegex(InstanceError, "head changed"):
            validate(self.base, self.instance_id, {})

    def test_missing_root_credentials_never_becomes_ready(self):
        self.prepare()
        result = self.cli("validate", "--instance", self.instance_id, "--require-credentials")
        self.assertEqual(result.returncode, 2)
        self.assertEqual(json.loads(result.stdout)["status"], "REJECTED")


if __name__ == "__main__":
    unittest.main()
