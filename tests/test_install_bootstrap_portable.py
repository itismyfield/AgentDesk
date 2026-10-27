import hashlib
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
INSTALL_SCRIPT = ROOT / "scripts" / "install.sh"
DASHBOARD_TOOLCHAIN_SCRIPT = ROOT / "scripts" / "check-dashboard-toolchain.sh"
DASHBOARD_INSTALL_STATE_SCRIPT = ROOT / "scripts" / "check-dashboard-install-state.mjs"
DASHBOARD_DEPENDENCY_INSTALL_SCRIPT = ROOT / "scripts" / "install-dashboard-dependencies.sh"


class InstallBootstrapPortableTests(unittest.TestCase):
    def read_script(self) -> str:
        return INSTALL_SCRIPT.read_text(encoding="utf-8")

    def test_installer_allows_repo_and_install_dir_overrides(self):
        text = self.read_script()

        self.assertIn('REPO="${AGENTDESK_INSTALL_REPO:-itismyfield/AgentDesk}"', text)
        self.assertIn('DEFAULT_INSTALL_DIR="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"', text)
        self.assertIn('INSTALL_DIR="${AGENTDESK_INSTALL_DIR:-$DEFAULT_INSTALL_DIR}"', text)
        self.assertIn('LAUNCHD_LABEL="${AGENTDESK_LAUNCHD_LABEL:-}"', text)
        self.assertIn('INSTALL_PORT="${AGENTDESK_INSTALL_PORT:-}"', text)
        self.assertIn("normalize_install_dir()", text)
        self.assertIn("default_install_port()", text)
        self.assertIn("agentdesk_supports_emit_launchd_label()", text)
        self.assertIn('if [ "$LAUNCHD_LABEL" = "com.agentdesk.release" ]; then', text)

    def test_release_scripts_honor_runtime_root_override(self):
        deploy_release = (ROOT / "scripts" / "deploy-release.sh").read_text(encoding="utf-8")
        deploy_dashboard = (ROOT / "scripts" / "deploy-dashboard.sh").read_text(encoding="utf-8")
        cli_wrapper = (ROOT / "scripts" / "ensure-agentdesk-cli.sh").read_text(encoding="utf-8")

        self.assertIn('ADK_REL="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"', deploy_release)
        self.assertIn('remote_root="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"', deploy_release)
        self.assertIn('ADK_REL="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"', deploy_dashboard)
        self.assertIn('runtime_root="\\${AGENTDESK_ROOT_DIR:-\\$home_dir/.adk/release}"', cli_wrapper)
        self.assertIn('"\\$runtime_root/bin/agentdesk"', cli_wrapper)

    def test_installer_creates_canonical_config_before_legacy_config(self):
        text = self.read_script()

        self.assertIn('CONFIG_PATH="$INSTALL_DIR/config/agentdesk.yaml"', text)
        self.assertIn('LEGACY_CONFIG_PATH="$INSTALL_DIR/agentdesk.yaml"', text)
        self.assertIn('cat > "$CONFIG_PATH" << YAML', text)
        self.assertNotIn('cat > "$INSTALL_DIR/agentdesk.yaml" << YAML', text)

    def test_native_runtime_help_uses_current_init_subcommand(self):
        text = self.read_script()

        self.assertIn("./target/release/agentdesk init", text)
        self.assertIn(".\\\\target\\\\release\\\\agentdesk.exe init", text)
        self.assertNotIn("agentdesk --init", text)
        self.assertNotIn("agentdesk.exe --init", text)

    def test_dashboard_install_and_deploy_fail_closed_on_toolchain_drift(self):
        install = self.read_script()
        deploy = (ROOT / "scripts" / "deploy-release.sh").read_text(encoding="utf-8")
        verify = (ROOT / "scripts" / "verify-dashboard.sh").read_text(encoding="utf-8")

        guard_call = 'bash "$SCRIPT_DIR/check-dashboard-toolchain.sh" "$REPO"'
        self.assertIn(guard_call, deploy)
        self.assertIn(guard_call, verify)
        self.assertIn('bash scripts/check-dashboard-toolchain.sh "$PWD"', install)
        self.assertIn('bash scripts/install-dashboard-dependencies.sh "$PWD/dashboard"', install)
        self.assertNotIn('npm run build 2>&1 | tail -1) || true', install)
        self.assertNotIn('node_modules/.bin/tsc', deploy)
        self.assertIn(
            'bash "$SCRIPT_DIR/install-dashboard-dependencies.sh" "$dashboard_dir"', deploy
        )
        self.assertIn(
            'bash "$SCRIPT_DIR/install-dashboard-dependencies.sh" "$DASHBOARD_DIR"', verify
        )
        dependency_install = DASHBOARD_DEPENDENCY_INSTALL_SCRIPT.read_text(encoding="utf-8")
        self.assertIn('npm ci --include=dev --no-audit --no-fund', dependency_install)
        self.assertIn('node "$SCRIPT_DIR/check-dashboard-install-state.mjs" "$dashboard_dir"', deploy)
        self.assertIn('Existing node_modules was preserved', deploy)

    def test_dashboard_filter_owns_all_toolchain_guard_inputs(self):
        workflow = (ROOT / ".github" / "workflows" / "ci-pr.yml").read_text(encoding="utf-8")
        dashboard_filter = workflow.split("            dashboard:\n", 1)[1].split(
            "            high_risk_recovery:\n", 1
        )[0]

        for path in (
            ".nvmrc",
            "dashboard/**",
            "scripts/check-dashboard-install-state.mjs",
            "scripts/check-dashboard-toolchain.sh",
            "scripts/install-dashboard-dependencies.sh",
            "scripts/verify-dashboard.sh",
        ):
            self.assertIn(f"- '{path}'", dashboard_filter)

    @unittest.skipIf(os.name == "nt", "behavioral dashboard installer uses POSIX paths")
    def test_dashboard_dependency_installer_includes_dev_under_production_env(self):
        if shutil.which("bash") is None:
            self.skipTest("bash is not available")

        with tempfile.TemporaryDirectory() as tmp:
            temp = Path(tmp)
            dashboard = temp / "dashboard"
            fakebin = temp / "fakebin"
            dashboard.mkdir()
            fakebin.mkdir()
            (dashboard / "package.json").write_text("{}\n", encoding="utf-8")
            (dashboard / "package-lock.json").write_text("{}\n", encoding="utf-8")
            self.write_executable(
                fakebin / "npm",
                """
                if [[ " $* " != *" ci "* || " $* " != *" --include=dev "* ]]; then
                  printf 'missing deterministic dev dependency flags: %s\n' "$*" >&2
                  exit 9
                fi
                """,
            )

            env = os.environ.copy()
            env.update(
                {
                    "PATH": f"{fakebin}:{env['PATH']}",
                    "NODE_ENV": "production",
                    "NPM_CONFIG_OMIT": "dev",
                }
            )
            result = subprocess.run(
                ["bash", str(DASHBOARD_DEPENDENCY_INSTALL_SCRIPT), str(dashboard)],
                cwd=ROOT,
                env=env,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )

            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    @unittest.skipIf(shutil.which("node") is None, "dashboard cache validation requires Node")
    def test_dashboard_install_state_requires_exact_lock_versions(self):
        with tempfile.TemporaryDirectory() as tmp:
            dashboard = Path(tmp) / "dashboard"
            package_dir = dashboard / "node_modules" / "example-package"
            package_dir.mkdir(parents=True)
            (dashboard / "package.json").write_text(
                '{"devDependencies":{"example-package":"1.0.0"}}\n', encoding="utf-8"
            )
            lock = {
                "lockfileVersion": 3,
                "packages": {
                    "": {"devDependencies": {"example-package": "1.0.0"}},
                    "node_modules/example-package": {
                        "version": "1.0.0",
                        "resolved": "https://registry.example/example-package.tgz",
                        "integrity": "sha512-test",
                    },
                },
            }
            (dashboard / "package-lock.json").write_text(json.dumps(lock), encoding="utf-8")
            installed_lock = {"lockfileVersion": 3, "packages": dict(lock["packages"])}
            installed_lock["packages"].pop("")
            (dashboard / "node_modules" / ".package-lock.json").write_text(
                json.dumps(installed_lock), encoding="utf-8"
            )
            (package_dir / "package.json").write_text(
                '{"name":"example-package","version":"1.0.0"}\n', encoding="utf-8"
            )

            exact = subprocess.run(
                ["node", str(DASHBOARD_INSTALL_STATE_SCRIPT), str(dashboard)],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(exact.returncode, 0, exact.stdout + exact.stderr)

            installed_lock["packages"]["node_modules/example-package"]["version"] = "0.9.0"
            (dashboard / "node_modules" / ".package-lock.json").write_text(
                json.dumps(installed_lock), encoding="utf-8"
            )
            stale = subprocess.run(
                ["node", str(DASHBOARD_INSTALL_STATE_SCRIPT), str(dashboard)],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertNotEqual(stale.returncode, 0)
            self.assertIn("version does not match package-lock.json", stale.stderr)

    def test_dashboard_pnpm_importer_matches_manifest_specs(self):
        manifest = json.loads((ROOT / "dashboard" / "package.json").read_text(encoding="utf-8"))
        lines = (ROOT / "dashboard" / "pnpm-lock.yaml").read_text(encoding="utf-8").splitlines()
        importer = {}
        current_group = None
        current_package = None
        in_root_importer = False

        for line in lines:
            if line == "  .:":
                in_root_importer = True
                continue
            if in_root_importer and line == "packages:":
                break
            if not in_root_importer:
                continue
            if line in ("    dependencies:", "    devDependencies:"):
                current_group = line.strip().removesuffix(":")
                importer[current_group] = {}
                current_package = None
                continue
            if line.startswith("    ") and not line.startswith("      "):
                current_group = None
                current_package = None
                continue
            if current_group and line.startswith("      ") and not line.startswith("        "):
                current_package = line.strip().removesuffix(":").strip("'\"")
                continue
            if current_group and current_package and line.startswith("        specifier: "):
                importer[current_group][current_package] = line.split("specifier: ", 1)[1].strip("'\"")

        for group in ("dependencies", "devDependencies"):
            self.assertEqual(importer.get(group), manifest.get(group), f"pnpm importer drift: {group}")

    @unittest.skipIf(os.name == "nt", "behavioral dashboard toolchain guard uses POSIX paths")
    def test_dashboard_toolchain_guard_rejects_an_old_node(self):
        if shutil.which("bash") is None:
            self.skipTest("bash is not available")

        with tempfile.TemporaryDirectory() as tmp:
            temp = Path(tmp)
            repo = temp / "repo"
            fakebin = temp / "fakebin"
            (repo / "dashboard").mkdir(parents=True)
            fakebin.mkdir()
            (repo / ".nvmrc").write_text("22.22.0\n", encoding="utf-8")
            (repo / "dashboard" / "package.json").write_text("{}\n", encoding="utf-8")
            (repo / "dashboard" / "package-lock.json").write_text("{}\n", encoding="utf-8")
            self.write_executable(
                fakebin / "node",
                """
                if [[ "${1:-}" == "-e" ]]; then
                  exit 1
                fi
                echo v22.21.0
                """,
            )
            self.write_executable(fakebin / "npm", "exit 0\n")

            env = os.environ.copy()
            env["PATH"] = f"{fakebin}:{env['PATH']}"
            result = subprocess.run(
                ["bash", str(DASHBOARD_TOOLCHAIN_SCRIPT), str(repo)],
                cwd=ROOT,
                env=env,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("requires Node >=22.22.0", result.stderr)
            self.assertIn("found v22.21.0", result.stderr)

    def installer_sandbox(self, mode):
        temporary = tempfile.TemporaryDirectory(prefix="agentdesk-installer-test-")
        temp = Path(temporary.name)
        runtime = temp / "fresh-home/.adk/sandbox-release"

        def cleanup():
            if sys.platform == "darwin":
                for path in temp.rglob("*"):
                    if path.is_file():
                        subprocess.run(["/usr/bin/chflags", "nouchg", str(path)], check=False,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            temporary.cleanup()

        self.addCleanup(cleanup)
        fakebin, home, tmpdir, fixture = (temp / name for name in ("fakebin", "fresh-home", "tmp", "fixture"))
        for path in (fakebin, home, tmpdir, fixture):
            path.mkdir(parents=True)
        manifest = ROOT / "scripts/runtime-assets.txt"
        contract = manifest.read_text(encoding="utf-8").splitlines()
        for relative in contract:
            if not relative or relative.startswith("#"):
                continue
            path = fixture / relative
            if relative in ("policies", "routines", "skills", "scripts/launchd-migrated"):
                path = path / "fixture.txt"
            path.parent.mkdir(parents=True, exist_ok=True)
            original = ROOT / relative
            if relative in ("scripts/runtime-assets.txt", "scripts/install_runtime_assets.py"):
                shutil.copyfile(original, path)
            else:
                content = "{}\n" if relative == "defaults.json" else "# distributed asset\n"
                path.write_text(content, encoding="utf-8")
        for helper in ("runtime-assets.txt", "install_runtime_assets.py"):
            original = ROOT / "scripts" / helper
            if original.exists():
                shutil.copyfile(original, fixture / "scripts" / helper)
        for relative, content in {
            "policies/lib/shared.js": "module.exports = { priority: 10 };\n",
            "policies/timeouts/constants.js": "module.exports = require('../lib/shared');\n",
            "policies/capabilities/auto-queue.yaml": "allow: []\n",
            "policies/auto-queue.capabilities.yaml": "allow: []\n",
            "policies/timeouts.capabilities.yaml": "allow: []\n",
            "policies/kanban-rules.capabilities.yaml": "allow: []\n",
            "routines/default.yaml": "name: fixture\n",
        }.items():
            path = fixture / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
        for name in ("auto-queue", "timeouts", "kanban-rules"):
            dependency = "./timeouts/constants" if name == "timeouts" else "./lib/shared"
            (fixture / f"policies/{name}.js").write_text(
                f"const dependency = require('{dependency}');\n"
                f"agentdesk.registerPolicy({{ name: '{name}', priority: dependency.priority }});\n",
                encoding="utf-8")
        (fixture / "dashboard/dist").mkdir(parents=True)
        (fixture / "dashboard/dist/index.html").write_text("<html>fixture</html>\n", encoding="utf-8")
        for helper in ("check-dashboard-toolchain.sh", "install-dashboard-dependencies.sh"):
            self.write_executable(fixture / "scripts" / helper, "exit 0\n")
        self.write_executable(fakebin / "npm", "exit 0\n")
        self.write_executable(fakebin / "uname", r"""
            case "${1:-}" in
              -s) echo Darwin ;;
              -m) echo arm64 ;;
              *) /usr/bin/uname "$@" ;;
            esac
        """)
        self.write_executable(fakebin / "git", r"""
            if [[ "${1:-}" == "clone" ]]; then
              target="${@: -1}"
              cp -R "$FIXTURE_ROOT" "$target"
              exit 0
            fi
            /usr/bin/git "$@"
        """)
        self.write_executable(fakebin / "cargo", r"""
            if [[ "${1:-}" == "build" ]]; then
              mkdir -p target/release
              cp "$FIXTURE_ROOT/agentdesk" target/release/agentdesk
            fi
        """)
        self.write_executable(fakebin / "codesign", r"""
            if [[ "${FAIL_INSTALL_SIGN:-0}" == 1 ]]; then
              echo sign > "$FIXTURE_FAILURE_LOG"
              exit 71
            fi
            if [[ "${FAIL_INSTALL_VERIFY:-0}" == 1 && "${1:-}" == -v ]]; then
              echo verify > "$FIXTURE_FAILURE_LOG"
              exit 73
            fi
        """)
        self.write_executable(fakebin / "mv", r"""
            args="$*"
            if [[ "${FAIL_INSTALL_RECOVERY:-0}" == 1 && "$args" == *"/bin/agentdesk" ]]; then
              if [[ "$args" == *"/previous "* ]]; then exit 75; fi
              /bin/mv "$@"
              exit 74
            fi
            if [[ "${FAIL_INSTALL_REPLACE:-0}" == 1 && "$args" == *".agentdesk-install."* && "$args" == *"/bin/agentdesk" ]]; then
              echo replace > "$FIXTURE_FAILURE_LOG"
              exit 72
            fi
            /bin/mv "$@"
        """)
        self.write_executable(fakebin / "launchctl", r"""
            if [[ "${1:-}" == "bootstrap" && "${KEEP_BOOT_INCARNATION:-0}" != 1 ]]; then
              count=0
              if [[ -f "$FIXTURE_LAUNCH_COUNT" ]]; then count="$(cat "$FIXTURE_LAUNCH_COUNT")"; fi
              echo "$((count + 1))" > "$FIXTURE_LAUNCH_COUNT"
            fi
        """)
        for command in ("sudo", "xattr", "open", "sleep"):
            self.write_executable(fakebin / command, "exit 0\n")
        if sys.platform != "darwin":
            self.write_executable(fakebin / "chflags", "exit 0\n")
        curl_body = r"""
import json, os, pathlib, shutil, subprocess, sys
args = sys.argv[1:]
request = " ".join(args)
if "releases/latest" in request:
    if os.environ["FIXTURE_MODE"] == "source":
        sys.exit(22)
    print(json.dumps({"tag_name": "v1.0.0"}))
elif "/api/health/detail" in request:
    counter = pathlib.Path(os.environ["FIXTURE_LAUNCH_COUNT"])
    if not counter.exists():
        sys.exit(22)
    print(json.dumps({"boot_incarnation": "fixture-boot-" + counter.read_text().strip()}))
elif "/api/health" in request:
    print(json.dumps({"status": "healthy"}, separators=(",", ":")))
elif "/api/policies" in request:
    # This API double resolves real fixture requires; it is not the QuickJS loader.
    root = pathlib.Path(os.environ["AGENTDESK_INSTALL_DIR"]).resolve() / "policies"
    program = r'''const path = require('path');
const root = process.argv[1];
const rows = [];
for (const name of ['auto-queue', 'timeouts', 'kanban-rules']) {
  global.agentdesk = {registerPolicy: policy => rows.push({name: policy.name, file: path.join(root, name + '.js')})};
  require(path.join(root, name + '.js'));
}
console.log(JSON.stringify(rows));'''
    result = subprocess.run(["node", "-e", program, str(root)], capture_output=True, text=True)
    if result.returncode:
        sys.stderr.write(result.stderr)
        sys.exit(22)
    policies = json.loads(result.stdout)
    for policy in policies:
        policy["file"] = str(root.parent) + "/./policies/" + policy["name"] + ".js"
    if os.environ.get("MISSING_INSTALL_POLICY"):
        policies = [p for p in policies if p["name"] != os.environ["MISSING_INSTALL_POLICY"]]
    if os.environ.get("WRONG_POLICY_ROOT"):
        for policy in policies:
            policy["file"] = "/other-runtime/policies/" + policy["name"] + ".js"
    print(json.dumps({"policies": policies}))
elif "-o" in args:
    shutil.copyfile(os.environ["FIXTURE_ARCHIVE"], args[args.index("-o") + 1])
else:
    sys.exit(22)
"""
        (fakebin / "curl").write_text(f"#!{sys.executable}\n" + textwrap.dedent(curl_body).lstrip(), encoding="utf-8")
        (fakebin / "curl").chmod(0o755)
        for args in (["init", "-q"], ["add", "."], ["-c", "user.name=Installer Test", "-c",
                     "user.email=installer@example.invalid", "commit", "-qm", "fixture"]):
            subprocess.run(["/usr/bin/git", "-C", str(fixture), *args], check=True, capture_output=True)
        env = os.environ.copy()
        env.update({
            "PATH": f"{fakebin}:{env['PATH']}", "HOME": str(home), "TMPDIR": str(tmpdir),
            "AGENTDESK_INSTALL_REPO": "example/AgentDesk", "AGENTDESK_INSTALL_DIR": str(runtime),
            "AGENTDESK_CODESIGN_IDENTITY": "-", "FIXTURE_ROOT": str(fixture), "FIXTURE_MODE": mode,
            "FIXTURE_ARCHIVE": str(temp / "release.tar.gz"),
            "FIXTURE_LAUNCH_COUNT": str(temp / "launch-count"),
            "FIXTURE_FAILURE_LOG": str(temp / "injected-failure"),
        })
        return temp, runtime, fixture, env

    def install_fixture(self, fixture, env, version="one"):
        self.write_executable(fixture / "agentdesk", r"""
            if [[ "${1:-}" == "--version" ]]; then
              echo 'agentdesk VERSION'
              exit 0
            fi
            if [[ "${1:-}" == "emit-launchd-plist" ]]; then
              if [[ "${2:-}" == "--help" ]]; then
                echo 'Usage: agentdesk emit-launchd-plist [--label <LABEL>]'
                exit 0
              fi
              output=""; label=""; root=""; home=""; bin=""
              while [[ "$#" -gt 0 ]]; do
                case "$1" in
                  --label) label="$2"; shift 2 ;;
                  --home) home="$2"; shift 2 ;;
                  --root-dir) root="$2"; shift 2 ;;
                  --agentdesk-bin) bin="$2"; shift 2 ;;
                  --output) output="$2"; shift 2 ;;
                  *) shift ;;
                esac
              done
              mkdir -p "$(dirname "$output")"
              cat > "$output" <<PLIST
            <plist><key>Label</key><string>${label}</string>
            <string>${home}</string><string>${root}</string><string>${bin}</string>
            <key>AGENTDESK_ROOT_DIR</key><key>AGENTDESK_DCSERVER_LABEL</key><string>${label}</string></plist>
            PLIST
            fi
        """.replace("VERSION", version))
        with tarfile.open(env["FIXTURE_ARCHIVE"], "w:gz") as archive:
            archive.add(fixture, arcname="agentdesk-darwin-aarch64",
                        filter=lambda info: None if ".git" in Path(info.name).parts else info)
        script = Path(env["TMPDIR"]) / "installer-snapshot.sh"
        script.write_bytes(INSTALL_SCRIPT.read_bytes())
        return subprocess.run(["/bin/bash", str(script)], cwd=ROOT, env=env, text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)

    def assert_installed_fixture(self, runtime, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for relative in (
            "policies/lib/shared.js", "policies/timeouts/constants.js",
            "policies/capabilities/auto-queue.yaml", "policies/auto-queue.capabilities.yaml",
            "routines/default.yaml", "scripts/queue-stability-batch.sh", "scripts/_defaults.sh",
        ):
            self.assertTrue((runtime / relative).is_file(), relative + "\n" + result.stdout)
        self.assertIn("Installation Complete", result.stdout)

    @unittest.skipIf(os.name == "nt" or shutil.which("node") is None, "POSIX installer fixture requires Node")
    def test_source_build_fallback_installs_into_fresh_sandbox(self):
        temp, runtime, fixture, env = self.installer_sandbox("source")
        result = self.install_fixture(fixture, env)
        self.assert_installed_fixture(runtime, result)
        home = temp / "fresh-home"
        config = runtime / "config/agentdesk.yaml"
        self.assertTrue(config.is_file(), result.stdout)
        self.assertFalse((runtime / "agentdesk.yaml").exists())
        self.assertFalse((home / "Library/LaunchAgents/com.agentdesk.release.plist").exists())
        plists = list((home / "Library/LaunchAgents").glob("com.agentdesk.release.sandbox-release.*.plist"))
        self.assertEqual(len(plists), 1, result.stdout)
        rendered = config.read_text() + plists[0].read_text()
        self.assertIn(str(runtime.resolve()), rendered)
        self.assertIn("<key>AGENTDESK_DCSERVER_LABEL</key>", rendered)
        self.assertNotIn("port: 8791", config.read_text())
        self.assertNotIn("/Users/itismyfield", rendered)
        self.assertNotIn("/Users/kunkun", rendered)

    @unittest.skipIf(os.name == "nt" or shutil.which("node") is None, "POSIX installer fixture requires Node")
    def test_release_installs_all_assets_into_fresh_sandbox(self):
        _, runtime, fixture, env = self.installer_sandbox("release")
        self.assert_installed_fixture(runtime, self.install_fixture(fixture, env))

    @unittest.skipUnless(sys.platform == "darwin" and shutil.which("node"), "real macOS immutable flags and Node required")
    def test_real_chflags_reinstall_preserves_operator_files(self):
        for mode in ("source", "release"):
            with self.subTest(mode=mode):
                _, runtime, fixture, env = self.installer_sandbox(mode)
                self.assert_installed_fixture(runtime, self.install_fixture(fixture, env))
                binary = runtime / "bin/agentdesk"
                previous = hashlib.sha256(binary.read_bytes()).hexdigest()
                protected = {
                    "config/agentdesk.yaml": "operator config\n",
                    "credentials/token": "operator credential\n",
                    "workspaces/user/notes.txt": "operator workspace\n",
                    "policies/operator.js": "operator policy\n",
                    "policies/auto-queue.js": (runtime / "policies/auto-queue.js").read_text() + "// operator edit\n",
                    "routines/default.yaml": "operator routine\n",
                    "routines/operator.yaml": "operator authored routine\n",
                    "skills/operator/SKILL.md": "operator skill\n",
                }
                for relative, content in protected.items():
                    path = runtime / relative
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text(content, encoding="utf-8")
                result = self.install_fixture(fixture, env, "two")
                self.assert_installed_fixture(runtime, result)
                self.assertNotEqual(hashlib.sha256(binary.read_bytes()).hexdigest(), previous)
                self.assertTrue(binary.stat().st_flags & 2, "second installation must restore uchg")
                print("REINSTALL", mode, "old_sha256=" + previous,
                      "new_sha256=" + hashlib.sha256(binary.read_bytes()).hexdigest(),
                      "version=agentdesk-two", "flags=" + str(binary.stat().st_flags))
                for relative, content in protected.items():
                    self.assertEqual((runtime / relative).read_text(), content, relative)

    @unittest.skipUnless(sys.platform == "darwin" and shutil.which("node"), "real macOS immutable flags and Node required")
    def test_reinstall_sign_or_replace_failure_preserves_binary_and_flags(self):
        for mode in ("source", "release"):
            for failure in ("FAIL_INSTALL_SIGN", "FAIL_INSTALL_VERIFY", "FAIL_INSTALL_REPLACE"):
                for initially_locked in (False, True):
                    with self.subTest(mode=mode, failure=failure, initially_locked=initially_locked):
                        _, runtime, fixture, env = self.installer_sandbox(mode)
                        first = self.install_fixture(fixture, env)
                        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
                        binary = runtime / "bin/agentdesk"
                        subprocess.run(["/usr/bin/chflags", "uchg" if initially_locked else "nouchg", str(binary)], check=True)
                        old_bytes, old_flags = binary.read_bytes(), binary.stat().st_flags
                        failed = self.install_fixture(fixture, {**env, failure: "1"}, "two")
                        self.assertNotEqual(failed.returncode, 0, failed.stdout + failed.stderr)
                        self.assertTrue(Path(env["FIXTURE_FAILURE_LOG"]).is_file(), failed.stdout + failed.stderr)
                        self.assertEqual(binary.read_bytes(), old_bytes)
                        self.assertEqual(binary.stat().st_flags, old_flags)
                        self.assertNotIn("Installation Complete", failed.stdout)
                        print("RECOVERY", mode, failure, "sha256=" + hashlib.sha256(old_bytes).hexdigest(),
                              "flags=" + str(binary.stat().st_flags))

    @unittest.skipUnless(sys.platform == "darwin" and shutil.which("node"), "real macOS immutable flags and Node required")
    def test_failed_rollback_retains_protected_candidate_and_previous_binary(self):
        for mode in ("source", "release"):
            with self.subTest(mode=mode):
                _, runtime, fixture, env = self.installer_sandbox(mode)
                first = self.install_fixture(fixture, env)
                self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
                binary = runtime / "bin/agentdesk"
                old_bytes, old_flags = binary.read_bytes(), binary.stat().st_flags
                failed = self.install_fixture(fixture, {**env, "FAIL_INSTALL_RECOVERY": "1"}, "two")
                self.assertNotEqual(failed.returncode, 0, failed.stdout + failed.stderr)
                self.assertIn("Binary recovery failed; previous binary retained at", failed.stdout)
                self.assertNotIn("Installation Complete", failed.stdout)
                self.assertTrue(binary.stat().st_flags & 2)
                backups = list((runtime / "bin").glob(".agentdesk-install.*/previous"))
                self.assertEqual(len(backups), 1)
                self.assertEqual(backups[0].read_bytes(), old_bytes)
                self.assertEqual(backups[0].stat().st_flags, old_flags)

    @unittest.skipUnless(sys.platform == "darwin" and shutil.which("node"), "macOS reinstall and Node required")
    def test_reinstall_requires_new_boot_incarnation(self):
        _, runtime, fixture, env = self.installer_sandbox("release")
        first = self.install_fixture(fixture, env)
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        binary = runtime / "bin/agentdesk"
        old_bytes, old_flags = binary.read_bytes(), binary.stat().st_flags
        failed = self.install_fixture(fixture, {**env, "KEEP_BOOT_INCARNATION": "1"}, "two")
        self.assertNotEqual(failed.returncode, 0, failed.stdout + failed.stderr)
        self.assertNotIn("Installation Complete", failed.stdout)
        self.assertEqual(binary.read_bytes(), old_bytes)
        self.assertEqual(binary.stat().st_flags, old_flags)

    @unittest.skipIf(os.name == "nt" or shutil.which("node") is None, "POSIX installer fixture requires Node")
    def test_policy_registration_gate_rejects_missing_or_wrong_root(self):
        for bad in ({"MISSING_INSTALL_POLICY": "timeouts"}, {"WRONG_POLICY_ROOT": "1"}):
            with self.subTest(bad=bad):
                _, runtime, fixture, env = self.installer_sandbox("release")
                failed = self.install_fixture(fixture, {**env, **bad})
                self.assertNotEqual(failed.returncode, 0, failed.stdout + failed.stderr)
                self.assertNotIn("Installation Complete", failed.stdout)

    def write_executable(self, path: Path, body: str) -> None:
        path.write_text("#!/usr/bin/env bash\nset -euo pipefail\n" + textwrap.dedent(body).lstrip(), encoding="utf-8")
        path.chmod(0o755)


if __name__ == "__main__":
    unittest.main()
