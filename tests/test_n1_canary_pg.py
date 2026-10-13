"""Required real-psql tests; run only inside pg_throwaway or the CI PG fixture."""
import json
import os
import subprocess
import sys
import unittest
from unittest.mock import patch
from urllib.parse import quote, urlencode, urlsplit, urlunsplit
import uuid

from tests import test_n1_canary_oracle as fixtures

sys.path.insert(0, str(fixtures.ROOT / "scripts"))
from n1_canary import Unknown
from n1_canary.evidence import PG_SELECT, pg_select
from n1_canary.audit import origins


class URIConnection(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if os.environ.get("AGENTDESK_REQUIRE_PG") != "1":
            raise AssertionError("required throwaway PG fixture is missing; never soft-skip")
        base = urlsplit(os.environ["POSTGRES_TEST_DATABASE_URL_BASE"])
        if base.hostname != "127.0.0.1" or not base.port:
            raise AssertionError("PG tests require the explicit loopback fixture")
        cls.database = "n1_oracle_fixture_" + uuid.uuid4().hex
        netloc = f"{quote(base.username or 'postgres')}@127.0.0.1:{base.port}"
        cls.uri = urlunsplit(("postgresql", netloc, "/" + cls.database, "", ""))
        admin = urlunsplit(("postgresql", netloc, "/postgres", "", ""))
        cls.port = base.port
        cls.scope = json.loads(fixtures.CONTRACT.read_text())["scope"]
        cls.psql(admin, f'CREATE DATABASE "{cls.database}";')
        cls.psql(cls.uri, """CREATE TABLE public.intake_outbox
            (id bigint, channel_id text, user_msg_id text, turn_kind text,
             status text, created_at timestamptz, completed_at timestamptz);
            INSERT INTO public.intake_outbox VALUES
            (1, '123', '901', 'user', 'completed', '2026-10-13T00:00:00Z', '2026-10-13T00:00:01Z'),
            (2, '124', '902', 'user', 'completed', '2026-10-13T00:00:00Z', '2026-10-13T00:00:01Z'),
            (3, '123', '903', 'user', 'completed', '2026-10-12T00:00:00Z', '2026-10-12T00:00:01Z'),
            (4, '9007199254740993', '904', 'user', 'completed', '2026-10-13T00:00:00Z', '2026-10-13T00:00:01Z');
            CREATE VIEW public.connection_settings AS SELECT
            'fixture-turn'::text AS turn_id, '123'::text AS channel_id, 'fixture'::text AS kind,
            json_build_object('database', current_database(), 'host', host(inet_server_addr()),
                'port', inet_server_port(), 'read_only', current_setting('default_transaction_read_only'),
                'timeout', current_setting('statement_timeout')) AS details_json,
            '2026-10-13T00:00:00Z'::timestamptz AS created_at
            UNION ALL SELECT 'large-turn', '9007199254740993', 'fixture', '{}'::json,
            '2026-10-13T00:00:00Z'::timestamptz;
            CREATE TABLE public.turn_lifecycle_events
            (turn_id text, channel_id text, kind text, details_json jsonb, created_at timestamptz);
            INSERT INTO public.turn_lifecycle_events SELECT turn_id, channel_id, kind,
            details_json::jsonb, created_at FROM public.connection_settings;""")

    @classmethod
    def psql(cls, uri, sql):
        result = subprocess.run(["psql", "-X", "-w", "--dbname", uri, "-v", "ON_ERROR_STOP=1"],
                                input=sql, text=True, capture_output=True, timeout=20)
        if result.returncode:
            raise AssertionError(result.stderr)

    def test_uri_reaches_expected_database_and_enforces_readonly(self):
        query = urlencode({"options": "-c default_transaction_read_only=off -c statement_timeout=0",
                           "connect_timeout": "60"}, quote_via=quote)
        # Conflicting ambient defaults reproduce the original PGDATABASE interpretation bug.
        with patch.dict(os.environ, {"N1_FIXTURE_DSN": self.uri + "?" + query,
                                    "PGDATABASE": "wrong_db", "PGHOST": "127.0.0.1", "PGPORT": "1"}), patch(
                "n1_canary.evidence.PG_SELECT", PG_SELECT.replace("public.turn_lifecycle_events", "public.connection_settings")):
            try:
                value = pg_select("N1_FIXTURE_DSN", self.scope)
            except Unknown as error:
                self.fail(f"URI collection failed: {error}")
        self.assertEqual([r["id"] for r in value["intake_outbox"]], [1])
        self.assertEqual(len(value["turn_lifecycle_events"]), 1)
        self.assertEqual(value["turn_lifecycle_events"][0]["details_json"], {
            "database": self.database, "host": "127.0.0.1", "port": self.port,
            "read_only": "on", "timeout": "10s"})

    def test_text_channels_reach_numeric_origin_audit(self):
        with patch.dict(os.environ, {"N1_FIXTURE_DSN": self.uri}):
            try:
                value = pg_select("N1_FIXTURE_DSN", self.scope)
            except Unknown as error:
                self.fail(f"TEXT schema collection failed: {error}")
        for rows in value.values():
            self.assertTrue(rows)
            self.assertTrue(all(type(row["channel_id"]) is int and row["channel_id"] == 123 for row in rows))
        identity = {"execution_nonce": "test", "pane": "test", "session": "test"}
        turns, proofs = [], []
        for n in range(51):
            physical = dict(start_offset=n, end_offset=n+1)
            turn = dict(physical_observation_key=physical, native_record_digest="test",
                        logical_turn_key={"native_turn_id": "fixture-turn" if n == 0 else str(n)}, outcome="completed")
            turns.append(turn)
            proofs.append(dict(physical_observation_key=physical, native_record_digest="test",
                               execution_identity=identity, input_kind="discord" if n == 0 else "tty",
                               discord_message_id="901" if n == 0 else None))
        manifest = dict(scope={**self.scope, "baseline_offset": 0, "fiftieth_offset": 51},
                        binding={"identity": identity}, discord={"bot_author_ids": [456]})
        legacy = dict(inputs=proofs, owner=dict(writer="o", turn_mode="n1", identity=identity))
        values = dict(pg=value, discord={"messages": [{"id": "901", "author": {"id": "789"}}]})
        eligible, inputs = origins({"observations": turns}, legacy, values, manifest)
        self.assertEqual((len(eligible), inputs), (50, {"901"}))

    def test_text_channel_above_float_precision_is_exact(self):
        channel = 9007199254740993
        with patch.dict(os.environ, {"N1_FIXTURE_DSN": self.uri}):
            value = pg_select("N1_FIXTURE_DSN", {**self.scope, "channel_id": channel})
        self.assertEqual([row["id"] for row in value["intake_outbox"]], [4])
        for rows in value.values():
            self.assertEqual(len(rows), 1)
            self.assertIs(type(rows[0]["channel_id"]), int)
            self.assertEqual(rows[0]["channel_id"], channel)

    def test_cli_uri_success_is_unknown_without_secret_output(self):
        fixture = fixtures.Fixture("test_valid_inputs_still_cannot_pass")
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        with patch.dict(os.environ, {"N1_FIXTURE_DSN": self.uri}):
            output = fixture.cli("--pg-dsn-env", "N1_FIXTURE_DSN")
        self.assertNotIn("pg_unavailable", output["reasons"])
        self.assertNotIn("pg_invalid", output["reasons"])
        self.assertNotIn(self.uri, json.dumps(output))
        with patch.dict(os.environ, {"N1_FIXTURE_DSN": self.uri.replace("@", ":fixture-secret@", 1)}):
            output = fixture.cli("--pg-dsn-env", "N1_FIXTURE_DSN")
        self.assertIn("pg_dsn_invalid", output["reasons"])
        self.assertNotIn("fixture-secret", json.dumps(output))


if __name__ == "__main__":
    unittest.main()
