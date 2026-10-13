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
from n1_canary.evidence import pg_select


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
            (id bigint, channel_id bigint, user_msg_id bigint, turn_kind text,
             status text, created_at timestamptz, completed_at timestamptz);
            CREATE VIEW public.turn_lifecycle_events AS SELECT
            'fixture-turn'::text AS turn_id, 123::bigint AS channel_id, 'fixture'::text AS kind,
            json_build_object('database', current_database(), 'host', host(inet_server_addr()),
                'port', inet_server_port(), 'read_only', current_setting('default_transaction_read_only'),
                'timeout', current_setting('statement_timeout')) AS details_json,
            '2026-10-13T00:00:00Z'::timestamptz AS created_at;""")

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
                                    "PGDATABASE": "wrong_db", "PGHOST": "127.0.0.1", "PGPORT": "1"}):
            try:
                value = pg_select("N1_FIXTURE_DSN", self.scope)
            except Unknown as error:
                self.fail(f"URI collection failed: {error}")
        self.assertEqual(value["intake_outbox"], [])
        self.assertEqual(len(value["turn_lifecycle_events"]), 1)
        self.assertEqual(value["turn_lifecycle_events"][0]["details_json"], {
            "database": self.database, "host": "127.0.0.1", "port": self.port,
            "read_only": "on", "timeout": "10s"})

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
