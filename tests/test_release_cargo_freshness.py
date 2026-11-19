"""Use one real Cargo cache for the direct and packaged release builds."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[1]


class ReleaseCargoFreshnessTests(unittest.TestCase):
    def test_direct_cargo_and_packaging_share_a_fresh_binary(self):
        with tempfile.TemporaryDirectory(prefix="release-cargo-cache-") as temp:
            root = Path(temp).resolve()
            repo = root / "checkout"
            scripts = repo / "scripts"
            (repo / "src/bin").mkdir(parents=True)
            scripts.mkdir()
            bin_dir = root / "bin"
            bin_dir.mkdir()
            cargo = shutil.which("cargo")
            self.assertIsNotNone(cargo)
            driver = bin_dir / "cargo"
            driver.write_text(f"""#!{sys.executable}
import os, sys
args = sys.argv[1:]
if args[:1] == ["build"]:
    args.append("--message-format=json")
os.execv({cargo!r}, [{cargo!r}, *args])
""")
            driver.chmod(0o700)
            for relative in ("build.rs", "scripts/build-release.sh", "scripts/package_release.py",
                             "scripts/install_runtime_assets.py", "scripts/release_source.py"):
                (repo / relative).write_bytes((REPO / relative).read_bytes())
            token = (REPO / "scripts/build_token.py").read_text()
            canonical = 'CANONICAL_TOKEN_PATH = "/tmp/adk-build-token.lock"'
            self.assertEqual(token.count(canonical), 1)
            (scripts / "build_token.py").write_text(token.replace(
                canonical, f"CANONICAL_TOKEN_PATH = {str(root / 'token')!r}"))
            (scripts / "_defaults.sh").write_text("setup_sccache_env() { return 1; }\n")
            (scripts / "check_postgres_migration_checksums.py").write_text("")
            (scripts / "runtime-assets.txt").write_text("scripts/install_runtime_assets.py\n")
            (repo / ".gitignore").write_text("/target/\n/dist/\n__pycache__/\n")
            (repo / "Cargo.toml").write_text(
                '[package]\nname="release-cargo-fixture"\nversion="0.1.0"\nedition="2021"\n'
            )
            source = repo / "src/bin/agentdesk.rs"
            source.write_text('fn main() { println!("release fixture"); }\n')
            readme = repo / "README.md"
            readme.write_text("fixture readme\n")
            migrations = repo / "migrations/postgres"
            migrations.mkdir(parents=True)
            (migrations / "base.sql").write_text("SELECT 1;\n")
            env = dict(os.environ, CARGO_TARGET_DIR=str(repo / "target"), CARGO_TERM_COLOR="never",
                       CARGO_NET_OFFLINE="true", PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
                       RUSTC_WRAPPER="", CARGO_BUILD_RUSTC_WRAPPER="", LC_ALL="C", TMPDIR=str(root))
            for name in ("ADK_RELEASE_CHECKOUT_SHA", "ADK_BUILD_TOKEN_HOLDER", "ADK_BUILD_TOKEN_LEASE"):
                env.pop(name, None)

            def run(*argv):
                result = subprocess.run(argv, cwd=repo, env=env, text=True,
                                        capture_output=True, timeout=30)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                return result.stdout

            run("git", "init", "-q")
            run("git", "config", "user.email", "fixture@example.invalid")
            run("git", "config", "user.name", "Cargo fixture")
            run("git", "config", "commit.gpgsign", "false")
            run("cargo", "generate-lockfile", "--offline")
            run("git", "add", ".")
            run("git", "commit", "-qm", "clean checkout")
            head = run("git", "rev-parse", "HEAD").strip()
            direct = ("cargo", "build", "--release")
            package = ("bash", "scripts/build-release.sh", "--skip-dashboard")

            def fresh(stdout):
                events = [json.loads(line) for line in stdout.splitlines() if line.startswith("{")]
                artifact, = [event for event in events
                             if event.get("reason") == "compiler-artifact"
                             and event["target"]["name"] == "agentdesk"]
                return artifact["fresh"]

            def read_package(witnessed):
                archive, = (repo / "dist").glob("agentdesk-*.tar.gz")
                prefix = archive.name.removesuffix(".tar.gz")
                with tarfile.open(archive) as bundle:
                    binary = bundle.extractfile(f"{prefix}/agentdesk").read()
                    name = f"{prefix}/agentdesk.source.json"
                    if witnessed:
                        data = json.load(bundle.extractfile(name))
                        self.assertEqual(data["source_sha"], head)
                        self.assertEqual(data["sha256"], hashlib.sha256(binary).hexdigest())
                    else:
                        self.assertNotIn(name, bundle.getnames())

            run(*direct)
            into_packaging = fresh(run(*package))
            into_direct = fresh(run(*direct))
            self.assertEqual((into_packaging, into_direct), (True, True),
                             "direct Cargo → packaging → direct Cargo")
            read_package(True)
            self.assertFalse((repo / "target/release/agentdesk.source.json").exists())

            readme.touch()
            (repo / ".git/index").touch()
            self.assertTrue(fresh(run(*package)))
            read_package(True)

            for dirty in (source, repo / "untracked"):
                original = dirty.read_text() if dirty.exists() else None
                dirty.write_text((original or "") + "// dirty checkout\n")
                run(*direct)
                self.assertTrue(fresh(run(*package)))
                read_package(False)
                if original is None:
                    dirty.unlink()
                else:
                    run("git", "restore", str(dirty))
                run(*package)
                read_package(True)


if __name__ == "__main__":
    unittest.main()
