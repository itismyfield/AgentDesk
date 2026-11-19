"""Use real Cargo on a tiny checkout through the supported release entrypoints."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[1]


class ReleaseCargoFreshnessTests(unittest.TestCase):
    def test_preparation_and_packaging_share_a_witness_and_a_fresh_binary(self):
        with tempfile.TemporaryDirectory(prefix="release-cargo-cache-") as temp:
            root = Path(temp).resolve()
            repo = root / "checkout"
            scripts = repo / "scripts"
            (repo / "src/bin").mkdir(parents=True)
            (repo / ".cargo").mkdir()
            scripts.mkdir()
            bin_dir = root / "bin"
            bin_dir.mkdir()
            codesign = bin_dir / "codesign"
            codesign.write_text("#!/bin/sh\nexit 0\n")
            codesign.chmod(0o700)
            for relative in ("Makefile", "build.rs", "scripts/build-release.sh"):
                (repo / relative).write_bytes((REPO / relative).read_bytes())
            token = (REPO / "scripts/build_token.py").read_text()
            canonical = 'CANONICAL_TOKEN_PATH = "/tmp/adk-build-token.lock"'
            self.assertEqual(token.count(canonical), 1)
            (scripts / "build_token.py").write_text(token.replace(
                canonical, f"CANONICAL_TOKEN_PATH = {str(root / 'token')!r}"))
            (scripts / "_defaults.sh").write_text("setup_sccache_env() { return 1; }\n")
            for stub in ("check_postgres_migration_checksums.py", "package_release.py"):
                (scripts / stub).write_text("")
            (repo / ".gitignore").write_text("/target/\n")
            (repo / ".cargo/config.toml").write_text("[term]\nverbose = true\n")
            (repo / "Cargo.toml").write_text(
                '[package]\nname="release-cargo-fixture"\nversion="0.1.0"\nedition="2021"\n'
            )
            source = repo / "src/bin/agentdesk.rs"
            source.write_text(
                'fn main() { println!("{}", option_env!("ADK_RUNNING_SOURCE_SHA").unwrap_or("")); }\n')
            readme = repo / "README.md"
            readme.write_text("fixture readme\n")
            migrations = repo / "migrations/postgres"
            migrations.mkdir(parents=True)
            (migrations / "base.sql").write_text("SELECT 1;\n")
            env = dict(os.environ, CARGO_TARGET_DIR=str(repo/"target"), CARGO_TERM_COLOR="never",
                       CARGO_NET_OFFLINE="true", PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
                       RUSTC_WRAPPER="", CARGO_BUILD_RUSTC_WRAPPER="", LC_ALL="C")
            for name in ("ADK_RELEASE_CHECKOUT_SHA", "ADK_BUILD_TOKEN_HOLDER", "ADK_BUILD_TOKEN_LEASE"):
                env.pop(name, None)

            def run(*argv):
                result = subprocess.run(argv, cwd=repo, env=env, text=True, capture_output=True, timeout=30)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                return result.stdout, result.stderr

            run("git", "init", "-q")
            run("git", "config", "user.email", "fixture@example.invalid")
            run("git", "config", "user.name", "Cargo fixture")
            run("git", "config", "commit.gpgsign", "false")
            run("cargo", "generate-lockfile", "--offline")
            run("git", "add", ".")
            run("git", "commit", "-qm", "clean checkout")
            head = run("git", "rev-parse", "HEAD")[0].strip()
            prep = ("make", "build")
            package = ("bash", "scripts/build-release.sh", "--skip-dashboard")
            binary = str(repo / "target/release/agentdesk")

            def fresh(*entrypoint):
                log = run(*entrypoint)[1]
                self.assertIn("Fresh release-cargo-fixture ", log)
                self.assertNotIn("Compiling release-cargo-fixture ", log)

            run(*prep)
            # Assert the cache handoffs before the output so the legacy path exposes the rebuild.
            fresh(*package)
            self.assertEqual(run(binary)[0].strip(), head)
            fresh(*prep)
            self.assertEqual(run(binary)[0].strip(), head)
            readme.touch()
            (repo / ".git/index").touch()
            fresh(*package)

            for dirty in (source, repo / "untracked"):
                original = dirty.read_text() if dirty.exists() else None
                dirty.write_text((original or "") + "// dirty checkout\n")
                run(*prep)
                self.assertEqual(run(binary)[0].strip(), "")
                fresh(*package)
                if original is None:
                    dirty.unlink()
                else:
                    run("git", "restore", str(dirty))
                run(*package)
                self.assertEqual(run(binary)[0].strip(), head)


if __name__ == "__main__":
    unittest.main()
