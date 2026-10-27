use super::*;
use serde_json::json;

#[test]
fn accuracy_code_mode_literal_calls_are_summarized_without_executing_js() {
    for code in [
        r#"const r = await tools.exec_command({cmd: "cargo check", yield_time_ms: 1000}); text(r);"#,
        r#"// @exec: {"max_output_tokens": 1000}
text(await tools.exec_command({"cmd":"cargo check"}));"#,
    ] {
        let out = safe_tool_summary("functions.exec", code, None).expect("literal JS call");
        assert!(out.contains("cargo check"), "{out}");
    }
    let out = safe_tool_summary("exec", r#"await Promise.all([tools.exec_command({cmd:'cargo check'}), tools.read_file({path:'src/lib.rs'})]);"#, None).unwrap();
    assert!(out.contains("Bash") && out.contains("Read"), "{out}");
    for code in [
        r#"const s = 'tools.exec_command({cmd:"FAKE_CALL"})';"#,
        "// tools.exec_command({cmd:'FAKE_CALL'})",
        "text(`tools.exec_command({cmd:'FAKE_CALL'})`)",
        "tools.exec_command({cmd: secretCommand})",
        "tools.exec_command({cmd: 'safe', ...override})",
        "tools.exec_command({cmd: `echo ${secret}`})",
        "tools.exec_command({cmd: 'gh auth login'})",
    ] {
        let out = safe_tool_summary("exec", code, None).unwrap_or_default();
        assert!(
            !out.contains("FAKE_CALL")
                && !out.contains("secret")
                && !out.contains("safe")
                && !out.contains("auth login"),
            "{out}"
        );
    }
}

#[test]
fn accuracy_shell_wrappers_with_options_keep_safe_commands() {
    for command in [
        "env -i bash -lc 'cargo check'",
        "timeout 30s bash -lc 'cargo check'",
        "command bash -lc 'cargo check' && git status",
    ] {
        let out =
            safe_tool_summary("Bash", &json!({"cmd":command}).to_string(), None).expect(command);
        assert!(out.contains("cargo check"), "{out}");
    }
    for command in [
        "env -i bash -lc 'redis-cli -a WrapperSentinel'",
        "timeout 30s bash -lc 'cargo check' && GH AUTH WrapperSentinel",
        "command bash -lc 'echo safe' | mysql WrapperSentinel",
    ] {
        assert!(
            safe_tool_summary("Bash", &json!({"cmd":command}).to_string(), None).is_none(),
            "{command}"
        );
    }
}

#[test]
fn accuracy_non_shell_credentials_use_wrappers_chains_and_case() {
    for command in [
        "env MYSQL NonshellSentinel",
        "echo ready; MYSQL NonshellSentinel",
        "timeout 30s /bin/Redis-CLI NonshellSentinel",
        "sudo GH AUTH NonshellSentinel",
        "env MONGOSH NonshellSentinel",
        "-mysql status; MYSQL NonshellSentinel",
        "'MYSQL NonshellSentinel'",
    ] {
        for raw in [command.to_string(), format!("- [ ] {command}")] {
            assert!(
                description_text(&raw, false).is_none(),
                "description: {raw}"
            );
            assert!(
                safe_section(&format!("Plan\n{raw}")).is_none(),
                "section: {raw}"
            );
        }
    }
    for safe in [
        "inspect mysql pooling",
        "echo mysql",
        "command -v mysql",
        "src/mysql.rs",
        "repair env handling",
        "-mysql status",
        "bash -lc 'cargo check'",
        "'inspect source'",
    ] {
        assert!(description_text(safe, false).is_some(), "{safe}");
    }
}

#[test]
fn accuracy_usernames_outside_paths_are_anonymous_without_hiding_commands() {
    for raw in [
        "username=LaneAlice cargo check",
        "user: LaneAlice",
        "ssh LaneAlice@example.test uptime",
        "ssh LaneAlice@mac-mini uptime",
        "scp a LaneAlice@10.0.0.2:/tmp",
        "ssh LaneAlice@localhost",
        "client --username LaneAlice",
        "client --owner LaneAlice /home/LaneAlice/src/lib.rs",
        "alice@example.test",
    ] {
        let out = text(raw, 120).unwrap();
        assert!(
            !out.contains("LaneAlice") && !out.contains("alice@"),
            "{out}"
        );
    }
    for raw in [
        "src/user.rs",
        "cargo test user_lookup",
        "git show feature/LaneAlice",
        "inspect users table",
        "user: src/user.rs",
        "curl --user public-user example.test",
        "npm i react@18",
        "npx -y create-vite@latest my-app",
        "cargo install ripgrep@14.1.0",
        "uvx ruff@latest check",
        "uvx ruff@0.6.0-rc.1 check",
        "inspect repo owner permissions",
        "inspect username handling",
    ] {
        assert_eq!(
            text(raw, 120).unwrap().replace('\u{200b}', ""),
            raw.replace('_', "\\_")
        );
        assert!(description_text(raw, true).is_some(), "{raw}");
    }
    for version in [
        "v18",
        "0.6",
        "1.2.3+build.4",
        "next",
        "beta",
        "alpha",
        "canary",
        "rc",
        "stable",
        "nightly",
        "lts",
    ] {
        let raw = format!("uvx package@{version} check");
        assert_eq!(text(&raw, 120).unwrap().replace('\u{200b}', ""), raw);
        assert!(description_text(&raw, true).is_some(), "{raw}");
    }
    assert!(description_text("alice@example.test", true).is_none());
}

#[test]
fn accuracy_code_mode_labels_escape_markdown_and_keep_the_first_label() {
    let out = safe_tool_summary("exec", "tools.exec_command({cmd:'cargo check'}); tools.view_image({path:'a.png'}); tools.write_stdin({session_id:1})", None).unwrap();
    assert!(out.starts_with("Bash · cargo check"), "{out}");
    assert!(
        out.contains("view\\_image") && out.contains("write\\_stdin"),
        "{out}"
    );
    let read = safe_tool_summary("exec", "tools.read_file({path:'src/lib.rs'})", None).unwrap();
    assert!(read.starts_with("Read · src/lib.rs"), "{read}");
}

#[test]
fn accuracy_account_operands_and_versioned_hosts_are_anonymous_while_versions_stay() {
    let bash = |command: &str| {
        safe_tool_summary("Bash", &json!({"command":command}).to_string(), None)
            .map(|out| out.replace('\u{200b}', ""))
    };
    for command in [
        "sudo -u LaneAlice cargo check",
        "sudo -E -u LaneAlice make",
        "sudo --user=LaneAlice make",
        "ssh -l LaneAlice build-host uptime",
        "ssh -p 22 -l LaneAlice build-host uptime",
        "su - LaneAlice",
        "su -c 'make check' LaneAlice",
        "chown LaneAlice:staff build.log",
        "chown -R LaneAlice target",
        "sudo chown LaneAlice build.log",
        "ssh LaneAlice@v2 uptime",
        "ssh LaneAlice@canary uptime",
        "rsync -a target LaneAlice@main:/srv",
        "echo LaneAlice@canary",
    ] {
        let out = bash(command).unwrap_or_default();
        assert!(!out.contains("LaneAlice"), "{command}: {out}");
    }
    for command in [
        "npm i node-lib@18.x",
        "pnpm add react@experimental",
        "npx next@canary build",
        "npm i some-lib@main",
        "uvx ruff@v0.6 check",
        "npm i react@18",
        "ssh -L 8080:localhost:80 build-host",
        "ssh build-host ls -la",
        "echo chown handling",
    ] {
        let out = bash(command).expect(command);
        assert!(out.contains(command), "{command}: {out}");
    }
}
