import os
import shutil
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
INSTALL_SCRIPT = ROOT / "scripts" / "install.sh"


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
