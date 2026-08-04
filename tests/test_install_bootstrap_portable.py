import json
import os
import shutil
import subprocess
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
        self.assertIn("AGENTDESK_ROOT_DIR \\", deploy_release)
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

    @unittest.skipIf(os.name == "nt", "behavioral bash installer smoke uses POSIX paths")
    def test_source_build_fallback_installs_into_fresh_sandbox(self):
        if shutil.which("bash") is None:
            self.skipTest("bash is not available")

        with tempfile.TemporaryDirectory() as tmp:
            temp = Path(tmp)
            fakebin = temp / "fakebin"
            home = temp / "fresh-home"
            runtime_root = home / ".adk" / "sandbox-release"
            tmpdir = temp / "tmp"
            fakebin.mkdir()
            home.mkdir()
            tmpdir.mkdir()

            self.write_executable(
                fakebin / "uname",
                """
                case "${1:-}" in
                  -s) echo Darwin ;;
                  -m) echo arm64 ;;
                  *) /usr/bin/uname "$@" ;;
                esac
                """,
            )
            self.write_executable(
                fakebin / "curl",
                """
                args="$*"
                if [[ "$args" == *"api.github.com/repos/example/AgentDesk/releases/latest"* ]]; then
                  exit 22
                fi
                if [[ "$args" == *"/api/health"* ]]; then
                  printf '{"status":"healthy"}\\n'
                  exit 0
                fi
                exit 0
                """,
            )
            self.write_executable(
                fakebin / "git",
                """
                if [[ "${1:-}" == "clone" ]]; then
                  target="${@: -1}"
                  mkdir -p "$target/policies"
                  printf 'agentdesk.registerPolicy({});\\n' > "$target/policies/default.js"
                  exit 0
                fi
                /usr/bin/git "$@"
                """,
            )
            self.write_executable(
                fakebin / "cargo",
                r"""
                if [[ "${1:-}" == "build" ]]; then
                  mkdir -p target/release
                  cat > target/release/agentdesk <<'SH'
                #!/usr/bin/env bash
                set -euo pipefail
                if [[ "${1:-}" == "emit-launchd-plist" ]]; then
                  if [[ "${2:-}" == "--help" ]]; then
                    printf 'Usage: agentdesk emit-launchd-plist [--label <LABEL>]\\n'
                    exit 0
                  fi
                  home=""
                  root=""
                  bin=""
                  output=""
                  label=""
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
                <plist>
                  <key>Label</key>
                  <string>${label}</string>
                  <string>${home}</string>
                  <string>${root}</string>
                  <string>${bin}</string>
                  <key>AGENTDESK_ROOT_DIR</key>
                  <key>AGENTDESK_DCSERVER_LABEL</key>
                  <string>${label}</string>
                </plist>
                PLIST
                  exit 0
                fi
                if [[ "${1:-}" == "dcserver" ]]; then
                  exit 0
                fi
                exit 0
                SH
                  chmod +x target/release/agentdesk
                  exit 0
                fi
                exit 0
                """,
            )
            for command in ("codesign", "chflags", "launchctl", "sudo", "xattr", "open"):
                self.write_executable(fakebin / command, "exit 0\n")

            env = os.environ.copy()
            env.update(
                {
                    "PATH": f"{fakebin}:{env['PATH']}",
                    "HOME": str(home),
                    "TMPDIR": str(tmpdir),
                    "AGENTDESK_INSTALL_REPO": "example/AgentDesk",
                    "AGENTDESK_INSTALL_DIR": str(runtime_root),
                    "AGENTDESK_CODESIGN_IDENTITY": "-",
                }
            )

            result = subprocess.run(
                ["bash", str(INSTALL_SCRIPT)],
                cwd=ROOT,
                env=env,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )

            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            normalized_runtime_root = runtime_root.resolve()
            config = normalized_runtime_root / "config" / "agentdesk.yaml"
            legacy_config = runtime_root / "agentdesk.yaml"
            default_plist = home / "Library" / "LaunchAgents" / "com.agentdesk.release.plist"
            sandbox_plists = sorted((home / "Library" / "LaunchAgents").glob("com.agentdesk.release.sandbox-release.*.plist"))

            self.assertTrue(config.is_file(), result.stdout)
            self.assertFalse(legacy_config.exists(), result.stdout)
            self.assertFalse(default_plist.exists(), result.stdout)
            self.assertEqual(len(sandbox_plists), 1, result.stdout)
            plist = sandbox_plists[0]
            self.assertTrue(plist.is_file(), result.stdout)
            self.assertIn(f"Config:     {config}", result.stdout)

            rendered = config.read_text(encoding="utf-8") + plist.read_text(encoding="utf-8")
            config_text = config.read_text(encoding="utf-8")
            self.assertIn(str(home), rendered)
            self.assertIn(str(normalized_runtime_root), rendered)
            self.assertNotIn("<string>com.agentdesk.release</string>", rendered)
            self.assertIn("<string>com.agentdesk.release.sandbox-release.", rendered)
            self.assertIn("<key>AGENTDESK_DCSERVER_LABEL</key>", rendered)
            self.assertNotIn("port: 8791", config_text)
            self.assertNotIn("/Users/itismyfield", rendered)
            self.assertNotIn("/Users/kunkun", rendered)
            self.assertNotIn("mac-mini-release", rendered)

    def write_executable(self, path: Path, body: str) -> None:
        path.write_text("#!/usr/bin/env bash\nset -euo pipefail\n" + textwrap.dedent(body).lstrip(), encoding="utf-8")
        path.chmod(0o755)


if __name__ == "__main__":
    unittest.main()
