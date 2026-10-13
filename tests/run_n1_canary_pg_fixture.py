"""Run URI tests with an explicit PG fixture; missing tools are failures, never skips."""
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def tests(env, module):
    subprocess.run([sys.executable, "-m", "unittest", "-v", module],
                   cwd=ROOT, env=env, check=True)
    subprocess.run([sys.executable, "tests/run_n1_canary_mutations.py"], cwd=ROOT, env=env, check=True)


def main():
    module = sys.argv[1] if len(sys.argv) == 2 else "tests.test_n1_canary_pg"
    if len(sys.argv) > 2 or module != "tests.test_n1_canary_pg":
        raise RuntimeError("only the N1 PG fixture module is accepted")
    env = dict(os.environ)
    if env.get("AGENTDESK_REQUIRE_PG") == "1":
        tests(env, module)
        return 0
    initdb = shutil.which("initdb") or next(
        (str(p) for p in sorted(Path("/usr/lib/postgresql").glob("*/bin/initdb"), reverse=True)), None)
    if not initdb:
        raise RuntimeError("N1_PG_FIXTURE_UNAVAILABLE: initdb is required")
    binary = Path(initdb).parent
    env = {k: v for k, v in env.items() if not k.startswith("PG")}
    env.update(PATH=str(binary) + os.pathsep + env["PATH"], LC_ALL="C", LANG="C")
    for tool in ("pg_ctl", "psql"):
        if not shutil.which(tool, path=env["PATH"]):
            raise RuntimeError(f"N1_PG_FIXTURE_UNAVAILABLE: {tool} is required")
    with tempfile.TemporaryDirectory(prefix="n1-oracle-pg-fixture-") as tmp:
        cluster = str(Path(tmp) / "cluster")
        subprocess.run([initdb, "-D", cluster, "-U", "postgres", "-A", "trust", "-E", "UTF8", "--locale=C"],
                       env=env, check=True, stdout=subprocess.DEVNULL)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        ctl = ["pg_ctl", "-D", cluster]
        try:
            subprocess.run([*ctl, "-o", f"-p {port} -c listen_addresses=127.0.0.1 -c unix_socket_directories=''",
                            "-l", str(Path(tmp) / "server.log"), "-w", "start"], env=env, check=True)
            env.update(AGENTDESK_REQUIRE_PG="1", POSTGRES_TEST_DATABASE_URL_BASE=f"postgres://postgres@127.0.0.1:{port}")
            tests(env, module)
        finally:
            subprocess.run([*ctl, "-m", "fast", "-w", "stop"], env=env, check=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
