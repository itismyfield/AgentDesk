use super::{class, safe_section, safe_tool_summary};
use serde_json::json;

fn render(name: &str, value: serde_json::Value) -> Option<String> {
    safe_tool_summary(name, &value.to_string(), Some("/work/project"))
}

#[test]
fn claude_and_codex_aliases_share_classes_and_summaries() {
    for name in [
        "Bash",
        "shell",
        "exec_command",
        "functions.exec_command",
        "shell_command",
    ] {
        assert_eq!(class(name), "Bash");
        assert_eq!(
            render(name, json!({"cmd":"cargo test -p agentdesk"})).as_deref(),
            Some("cargo test -p agentdesk")
        );
        assert_eq!(
            render(
                name,
                json!(json!({"cmd":"cargo test -p agentdesk"}).to_string())
            )
            .as_deref(),
            Some("cargo test -p agentdesk")
        );
    }
    for (name, expected) in [
        ("Read", "Read"),
        ("read_file", "Read"),
        ("Edit", "Edit"),
        ("MultiEdit", "Edit"),
        ("NotebookEdit", "Edit"),
        ("apply_patch", "Edit"),
        ("Write", "Write"),
        ("write_file", "Write"),
        ("Grep", "Grep"),
        ("search_files", "Grep"),
        ("Glob", "Glob"),
        ("Agent", "Task"),
        ("Task", "Task"),
        ("WebFetch", "WebFetch"),
        ("web_search", "WebSearch"),
        ("mcp__memento__recall", "MCP"),
        ("unknown @everyone", "Tool"),
    ] {
        assert_eq!(class(name), expected, "{name}");
    }
    assert_eq!(
        render("Read", json!({"file_path":"/work/project/src/main.rs"})).as_deref(),
        Some("src/main.rs")
    );
    assert_eq!(
        render(
            "Edit",
            json!({"file_path":"src/main.rs","old_string":"old\nline","new_string":"new"})
        )
        .as_deref(),
        Some("src/main.rs (+1 −2)")
    );
    assert_eq!(
        render(
            "MultiEdit",
            json!({"file_path":"src/main.rs","edits":[{"old_string":"a","new_string":"b\nc"}]})
        )
        .as_deref(),
        Some("src/main.rs (+2 −1)")
    );
    assert_eq!(
        render(
            "Write",
            json!({"file_path":"src/main.rs","content":"PRIVATE CONTENT"})
        )
        .as_deref(),
        Some("src/main.rs")
    );
    assert_eq!(
        render(
            "NotebookEdit",
            json!({"notebook_path":"notebooks/demo.ipynb","new_source":"PRIVATE CONTENT"})
        )
        .as_deref(),
        Some("notebooks/demo.ipynb")
    );
    assert_eq!(
        render("Grep", json!({"pattern":"tool","path":"/work/project/src"})).as_deref(),
        Some("\"tool\" · src")
    );
    assert_eq!(
        render("Glob", json!({"pattern":"*.rs"})).as_deref(),
        Some("\"\\*.rs\"")
    );
    assert_eq!(
        render("Task", json!({"description":"inspect source\nSECOND LINE"})).as_deref(),
        Some("inspect source")
    );
    assert_eq!(
        render("WebSearch", json!({"query":"rust language"})).as_deref(),
        Some("rust language")
    );
}

#[test]
fn raw_and_json_apply_patch_show_only_paths_and_line_counts() {
    let patch = "*** Begin Patch\n*** Update File: /work/project/src/main.rs\n@@\n-PRIVATE OLD\n+PRIVATE NEW\n+SECOND\n*** End Patch\n";
    for input in [
        patch.to_string(),
        json!({"patch":patch}).to_string(),
        json!(patch).to_string(),
    ] {
        assert_eq!(
            safe_tool_summary("apply_patch", &input, Some("/work/project")).as_deref(),
            Some("src/main.rs (+2 −1)")
        );
    }
    assert!(safe_tool_summary("apply_patch", "not a patch", None).is_none());
    let private_patch = "*** Begin Patch\n*** Add File: .env.local\n+PRIVATE VALUE\n*** End Patch";
    assert_eq!(
        safe_tool_summary("apply_patch", private_patch, None).as_deref(),
        Some("[비공개 파일] (+1 −0)")
    );
}

#[test]
fn secret_corpus_masks_each_value_and_quoted_fragment() {
    // These short sentinels also isolate flag/userinfo/token mutation checks:
    // assignment and >=32-character opaque rules cannot hide their omissions.
    let cases: &[(&str, &[&str])] = &[
        (
            "curl -H 'Authorization: Basic dXNlcjpwYXNz' https://example.test",
            &["dXNlcjpwYXNz"],
        ),
        (
            "curl -H 'aUtHoRiZaTiOn: Bearer HEADERLEAK' https://example.test",
            &["HEADERLEAK"],
        ),
        (
            "curl -H 'Authorization: CUSTOMLEAK' https://example.test",
            &["CUSTOMLEAK"],
        ),
        ("echo Basic BASIC_LEAK", &["BASIC_LEAK"]),
        ("echo Bearer BEARER_LEAK", &["BEARER_LEAK"]),
        (
            "mysql --password 'FLAG_HEAD FLAG_TAIL'",
            &["FLAG_HEAD", "FLAG_TAIL"],
        ),
        ("mysql -pSHORTPWD", &["SHORTPWD"]),
        (
            "cat '/Users/SPACE_HEAD SPACE_TAIL/file.rs'",
            &["SPACE_HEAD", "SPACE_TAIL"],
        ),
        (
            "mysql --pass\"word\" OBFUSCATED_VALUE",
            &["OBFUSCATED_VALUE"],
        ),
        ("echo to\"ken\"=OBFUSCATED_TOKEN", &["OBFUSCATED_TOKEN"]),
        ("echo gh\"p\"_QUOTE_LEAK9", &["QUOTE_LEAK9"]),
        (
            "curl ht\"tps\"://QUOTE_USER:QUOTE_PASS@example.test/path",
            &["QUOTE_USER", "QUOTE_PASS"],
        ),
        ("cat /Us\"ers\"/FAKE_ACCOUNT/file.rs", &["FAKE_ACCOUNT"]),
        (
            "mysql --PaSsWoRd='FLAG_HEAD'\"FLAG_TAIL\"",
            &["FLAG_HEAD", "FLAG_TAIL"],
        ),
        (
            "echo \"token=WRAPPED_HEAD WRAPPED_TAIL\"",
            &["WRAPPED_HEAD", "WRAPPED_TAIL"],
        ),
        (
            "password = 'SPACED_HEAD SPACED_TAIL' echo safe",
            &["SPACED_HEAD", "SPACED_TAIL"],
        ),
        ("passwd PASSWD_LEAK", &["PASSWD_LEAK"]),
        ("token=TOKEN_LEAK", &["TOKEN_LEAK"]),
        ("api_key=API_LEAK", &["API_LEAK"]),
        ("secret=SECRET_LEAK", &["SECRET_LEAK"]),
        (
            "export SERVICE_KEY=ENV_KEY_LEAK echo safe",
            &["ENV_KEY_LEAK"],
        ),
        ("export SERVICE_TOKEN=ENV_TOKEN_LEAK", &["ENV_TOKEN_LEAK"]),
        (
            "export SERVICE_SECRET=ENV_SECRET_LEAK",
            &["ENV_SECRET_LEAK"],
        ),
        (
            "export MY_PASSWORD_VALUE=ENV_PASSWORD_LEAK",
            &["ENV_PASSWORD_LEAK"],
        ),
        (
            "curl https://SHORTUSER:SHORTPASS@example.test/path",
            &["SHORTUSER", "SHORTPASS"],
        ),
        (
            "curl 'https://example.test/path?X-Amz-Signature=AMZLEAK&sig=SIGLEAK&token=QUERYLEAK'",
            &["AMZLEAK", "SIGLEAK", "QUERYLEAK"],
        ),
        ("echo ghp_ShortFakeLeak9", &["ghp_ShortFakeLeak9"]),
        ("echo gho_ShortFakeLeak9", &["gho_ShortFakeLeak9"]),
        (
            "echo github_pat_ShortFakeLeak9",
            &["github_pat_ShortFakeLeak9"],
        ),
        ("echo sk-FakeLeak123", &["sk-FakeLeak123"]),
        ("echo sk-ant-FakeLeak123", &["sk-ant-FakeLeak123"]),
        ("echo xoxb-FakeLeak123", &["xoxb-FakeLeak123"]),
        ("echo xoxp-FakeLeak123", &["xoxp-FakeLeak123"]),
        ("echo AKIAFAKELEAK123", &["AKIAFAKELEAK123"]),
        ("echo ASIAFAKELEAK123", &["ASIAFAKELEAK123"]),
        ("echo AIzaFakeLeak123", &["AIzaFakeLeak123"]),
        (
            "echo eyJhbGciOiJub25lIn0.eyJpZCI6MTIzfQ.fake",
            &["eyJhbGciOiJub25lIn0", "eyJpZCI6MTIzfQ", ".fake"],
        ),
        (
            "echo abcdef0123456789abcdef0123456789",
            &["abcdef0123456789abcdef0123456789"],
        ),
        (
            "echo QWxwaGFCZXRhR2FtbWFEZWx0YUVwc2lsb24=",
            &["QWxwaGFCZXRhR2FtbWFEZWx0YUVwc2lsb24="],
        ),
        (
            "echo -----BEGIN PRIVATE KEY-----\nPEMLEAK\n-----END PRIVATE KEY-----",
            &["PEMLEAK", "BEGIN PRIVATE KEY"],
        ),
        (
            "TOKEN=$(security find-generic-password -s KEYCHAINLEAK -w)",
            &["KEYCHAINLEAK"],
        ),
        (
            "curl https://ENCUSER:ENCPASS@example.test/%67%68%70_LEAK",
            &["ENCUSER", "ENCPASS", "LEAK"],
        ),
        ("echo safe\necho SECOND_LINE_LEAK", &["SECOND_LINE_LEAK"]),
        ("cat <<EOF\nHEREDOC_LEAK\nEOF", &["HEREDOC_LEAK"]),
        (
            "echo token='UNTERMINATED_HEAD UNTERMINATED_TAIL",
            &["UNTERMINATED_HEAD", "UNTERMINATED_TAIL"],
        ),
        (
            "echo TOKEN=$'ANSI_HEAD ANSI_TAIL'",
            &["ANSI_HEAD", "ANSI_TAIL"],
        ),
        (
            "curl --header 'Cookie: COOKIE_HEAD COOKIE_TAIL'",
            &["COOKIE_HEAD", "COOKIE_TAIL"],
        ),
        (
            "curl -u admin:PairLeak9 example.test",
            &["admin", "PairLeak9"],
        ),
        (
            "curl --user admin:PairLeak9 example.test",
            &["admin", "PairLeak9"],
        ),
        ("sshpass -p CmdLeak9 ssh host", &["CmdLeak9"]),
        ("MYSQL_PWD=EnvLeak9 client", &["EnvLeak9"]),
        ("DB_PASS=EnvLeak9 client", &["EnvLeak9"]),
        ("redis-cli -a CmdLeak9", &["CmdLeak9"]),
        ("export GITLAB=glpat-SmallLeak9", &["glpat-SmallLeak9"]),
        ("htpasswd -b f admin CmdLeak9", &["CmdLeak9"]),
        (
            "echo CmdLeak9 | docker login --password-stdin",
            &["CmdLeak9"],
        ),
        ("the password is ProseLeak9", &["ProseLeak9"]),
        ("SERVICE_AUTH=EnvLeak9 client", &["EnvLeak9"]),
        ("SERVICE_CRED=EnvLeak9 client", &["EnvLeak9"]),
        ("client --api-key APILEAK9", &["APILEAK9"]),
        ("inspect glptt-SmallLeak9", &["glptt-SmallLeak9"]),
        ("inspect npm_SmallLeak9", &["npm_SmallLeak9"]),
        ("inspect pypi-SmallLeak9", &["pypi-SmallLeak9"]),
        ("inspect hf_SmallLeak9", &["hf_SmallLeak9"]),
        ("inspect xapp-SmallLeak9", &["xapp-SmallLeak9"]),
        ("inspect xoxa-SmallLeak9", &["xoxa-SmallLeak9"]),
        ("inspect xoxr-SmallLeak9", &["xoxr-SmallLeak9"]),
        ("inspect shpat_SmallLeak9", &["shpat_SmallLeak9"]),
        ("inspect SG.SmallLeak9.part", &["SG.SmallLeak9.part"]),
        ("echo $HOME ${PROJECT} ghp_RefLeak9", &["ghp_RefLeak9"]),
        ("DB_PASS=RefLeak9 echo $HOME", &["RefLeak9"]),
        ("echo $(client --password SubLeak9)", &["SubLeak9"]),
        ("echo `client --password TickLeak9`", &["TickLeak9"]),
        (
            "echo $HOME https://UsrLeak9:PwdLeak9@example.test",
            &["UsrLeak9", "PwdLeak9"],
        ),
        ("curl $HOST/path?sig=SignedLeak9", &["SignedLeak9"]),
        ("npm config set _authToken SpacedLeak9", &["SpacedLeak9"]),
        (
            "rclone config secret_access_key SpacedLeak9",
            &["SpacedLeak9"],
        ),
        ("the password was ProseLeak9", &["ProseLeak9"]),
        ("비밀번호는 KoreanLeak9", &["KoreanLeak9"]),
        ("비밀번호: KoreanLeak9", &["KoreanLeak9"]),
        ("echo $ghp_RefTokenLeak9 $HOME", &["RefTokenLeak9"]),
        ("echo ${ghp_RefTokenLeak9} $HOME", &["RefTokenLeak9"]),
        ("echo $sk-RefTokenLeak9 $HOME", &["RefTokenLeak9"]),
        ("export $KEY=RefAssignLeak9", &["RefAssignLeak9"]),
    ];
    assert_eq!(cases.len(), 84);
    for (command, secrets) in cases {
        for tool in ["Bash", "exec_command", "Task", "WebSearch"] {
            let input =
                json!({"command":command,"cmd":command,"description":command,"query":command});
            for value in [input.clone(), json!(input.to_string())] {
                let output = render(tool, value).unwrap_or_default();
                let visible = output.replace(['\\', '\u{200b}'], "");
                for secret in *secrets {
                    assert!(
                        !output.contains(secret) && !visible.contains(secret),
                        "{tool}: {secret:?} survived in {output:?}"
                    );
                }
            }
        }
    }
    for command in [
        "client --password 'NORMAL_HEAD NORMAL_TAIL'",
        "client --password='NORMAL_HEAD'\"NORMAL_TAIL\"",
        "echo 'ordinary quoted words'",
    ] {
        assert!(
            render("Bash", json!({"command":command})).is_some(),
            "{command}"
        );
    }
}

#[test]
fn repair_credential_commands_show_only_class() {
    for command in [
        "sshpass -p CmdLeak9 ssh host",
        "htpasswd -b f user CmdLeak9",
        "redis-cli -a CmdLeak9",
        "mysql -e query",
        "mysqladmin ping",
        "psql -c query",
        "docker login registry",
        "podman login registry",
        "gh auth status",
        "op read item",
        "vault kv get item",
        "aws configure get region",
        "echo CmdLeak9 | client --password-stdin",
        "/usr/bin/mysql -e query",
        "sudo psql -c query",
        "env MODE=dev sshpass -p CmdLeak9 ssh host",
        "my\"sql\" -e query",
        "docker lo\"gin\" registry",
        "sudo -u root env MODE='two words' /usr/bin/psql -c query",
        "sh -c 'redis-cli -a CmdLeak9'",
        "sudo --chroot /tmp redis-cli -a CmdLeak9",
        "env -S 'redis-cli -a CmdLeak9'",
    ] {
        for tool in ["Bash", "exec_command"] {
            assert!(
                render(tool, json!({"command":command})).is_none(),
                "{tool}: {command}"
            );
        }
    }
    assert!(render("Bash", json!({"command":"echo ordinary-command"})).is_some());
    assert_eq!(
        safe_section("**Plan**\n- [ ] fix mysql loader").as_deref(),
        Some("**Plan**\n- [ ] fix mysql loader")
    );
    assert!(render("Bash", json!({"command":"echo ';' mysql loader"})).is_some());
    assert!(safe_section("**Tasks**\n└ redis-cli -a CmdLeak9").is_none());
    assert!(safe_section("Tasks\n└ Bash Launch codex").is_some());
    assert!(safe_section("Tasks\n└ Bash cargo test -p ordinary\\_crate · running").is_some());
    for title in ["redis-cli -a CmdLeak9", "-c 'redis-cli -a CmdLeak9'"] {
        assert!(safe_section(&format!("Tasks\n└ Bash {title}")).is_none());
    }
}

#[test]
fn repair_userpass_arguments_hide_both_halves() {
    for command in [
        "curl -u UserLeak9:PairLeak9 example.test",
        "curl --user UserLeak9:PairLeak9 example.test",
        "curl --user=UserLeak9:PairLeak9 example.test",
        "curl -u'UserLeak9:PairLeak9 tail' example.test",
        "curl --us\"er\" UserLeak9:PairLeak9 example.test",
    ] {
        for tool in ["Bash", "Task", "WebSearch"] {
            let output = render(
                tool,
                json!({"command":command,"description":command,"query":command}),
            )
            .unwrap_or_default();
            let visible = output.replace(['\\', '\u{200b}'], "");
            assert!(
                !visible.contains("UserLeak9") && !visible.contains("PairLeak9"),
                "{output}"
            );
        }
    }
    assert!(
        render(
            "Bash",
            json!({"command":"curl --user public-user example.test"})
        )
        .unwrap()
        .contains("public-user")
    );
    assert!(
        render(
            "Bash",
            json!({"command":"curl -u UserLeak9:PairLeak9 example.test"})
        )
        .is_some()
    );
}

#[test]
fn repair_abbreviated_environment_names_mask_values() {
    for command in [
        "DB_PASS=EnvLeak9 client",
        "MYSQL_PWD=EnvLeak9 client",
        "SERVICE_AUTH=EnvLeak9 client",
        "SERVICE_CRED=EnvLeak9 client",
        "export DB_PASS = 'ENV_HEAD ENV_TAIL' client",
        "echo \"SERVICE_AUTH=ENV_HEAD ENV_TAIL\"",
        "DB_PA\"SS\"=EnvLeak9 client",
    ] {
        let output = render("Bash", json!({"command":command})).unwrap_or_default();
        let visible = output.replace(['\\', '\u{200b}'], "");
        for secret in ["EnvLeak9", "ENV_HEAD", "ENV_TAIL"] {
            assert!(!visible.contains(secret), "{command}: {output}");
        }
    }
    assert!(render("Bash", json!({"command":"SERVICE_AUTH=EnvLeak9 client"})).is_none());
    assert_eq!(
        safe_section("**Plan**\n- [ ] fix author\\_module and credit\\_loader").as_deref(),
        Some("**Plan**\n- [ ] fix author\\_module and credit\\_loader")
    );
    assert_eq!(
        safe_section("fix DB\\_AUTH loader").as_deref(),
        Some("fix DB\\_AUTH loader")
    );
    for tool in ["Task", "WebSearch"] {
        for phrase in [
            "DB_AUTH=",
            "DB_AUTH='ENV_HEAD ENV_TAIL",
            "DB_AUTH=\"ENV_HEAD ENV_TAIL",
        ] {
            assert!(
                render(tool, json!({"description":phrase,"query":phrase})).is_none(),
                "{tool}: {phrase}"
            );
        }
    }
}

#[test]
fn repair_additional_token_prefixes_mask_short_values() {
    for token in [
        "glpat-SmallLeak9",
        "glptt-SmallLeak9",
        "npm_SmallLeak9",
        "pypi-SmallLeak9",
        "hf_SmallLeak9",
        "xapp-SmallLeak9",
        "xoxa-SmallLeak9",
        "xoxr-SmallLeak9",
        "shpat_SmallLeak9",
        "SG.SmallLeak9.part",
    ] {
        let output = render("Task", json!({"description":format!("inspect {token}")})).unwrap();
        assert!(
            !output
                .replace(['\\', '\u{200b}'], "")
                .contains("SmallLeak9"),
            "{output}"
        );
    }
    let output = render(
        "Task",
        json!({"description":"inspect gl\"pat\"-SmallLeak9"}),
    )
    .unwrap_or_default();
    assert!(!output.contains("SmallLeak9"));
}

#[test]
fn repair_natural_language_credentials_mask_the_value() {
    for tool in ["Task", "WebSearch"] {
        for phrase in [
            "the password is ProseLeak9",
            "the passwd is ProseLeak9",
            "the secret is ProseLeak9",
            "the token is ProseLeak9",
            "the token is 'PROSE_HEAD PROSE_TAIL'",
            "the password: ProseLeak9",
            "the password=ProseLeak9",
            "the password is: ProseLeak9",
            "the password: is ProseLeak9",
        ] {
            let output = render(tool, json!({"description":phrase,"query":phrase})).unwrap();
            let visible = output.replace(['\\', '\u{200b}'], "");
            for secret in ["ProseLeak9", "PROSE_HEAD", "PROSE_TAIL"] {
                assert!(!visible.contains(secret), "{output}");
            }
        }
    }
}

#[test]
fn repair_all_tool_summaries_disable_url_schemes_after_redaction() {
    let cases = [
        ("Bash", json!({"command":"curl https://example.test/docs"})),
        (
            "Read",
            json!({"file_path":"docs/https://example.test/file"}),
        ),
        (
            "Edit",
            json!({"file_path":"docs/https://example.test/file","old_string":"a","new_string":"b"}),
        ),
        (
            "Write",
            json!({"file_path":"docs/https://example.test/file"}),
        ),
        (
            "Grep",
            json!({"pattern":"https://example.test/docs","path":"src"}),
        ),
        ("Glob", json!({"pattern":"https://example.test/*"})),
        (
            "Task",
            json!({"description":"inspect https://example.test/docs"}),
        ),
        (
            "WebSearch",
            json!({"query":"inspect https://example.test/docs"}),
        ),
        (
            "WebFetch",
            json!({"url":"https://example.test/http://mirror.test/file"}),
        ),
        (
            "mcp__server__tool",
            json!({"url":"https://ARGUMENT_LEAK.test"}),
        ),
        (
            "apply_patch",
            json!({"patch":"*** Begin Patch\n*** Update File: docs/https://example.test/file\n+a\n*** End Patch"}),
        ),
    ];
    for (tool, input) in cases {
        let output = render(tool, input).unwrap();
        assert!(!output.contains("://"), "{tool}: {output}");
    }
    let output = render(
        "Bash",
        json!({"command":"curl https://UsrLeak9:PssLeak9@example.test/path?sig=SignLeak9"}),
    )
    .unwrap();
    let visible = output.replace(['\\', '\u{200b}'], "");
    for secret in ["UsrLeak9", "PssLeak9", "SignLeak9"] {
        assert!(!visible.contains(secret), "{output}");
    }
    assert!(
        !safe_section("**Tools**\nhttps://example.test/docs")
            .unwrap()
            .contains("://")
    );
}

#[test]
fn paths_are_relative_private_or_anonymous_before_display() {
    for path in [
        ".env",
        ".env.production",
        ".SSH/config",
        "keys/file.PEM",
        "keys/file.key",
        "data/credentials.json",
        "data/secrets/config",
        "login.keychain-db",
        ".ssh/../../src/file.rs",
        "C:\\Users\\FAKEUSER\\.ssh\\config",
    ] {
        let output = render("Read", json!({"file_path":path})).unwrap();
        assert_eq!(output, "[비공개 파일]", "{path}");
    }
    for path in [
        "/Users/FAKEUSER/file.rs",
        "/home/FAKEUSER/file.rs",
        "C:\\Users\\FAKEUSER\\file.rs",
    ] {
        let output = render("Read", json!({"file_path":path})).unwrap();
        assert!(!output.contains("FAKEUSER"), "{output}");
    }
    for path in [
        "/Users/SPACE_HEAD SPACE_TAIL/file.rs",
        "/home/SPACE_HEAD SPACE_TAIL/file.rs",
        "C:\\Users\\SPACE_HEAD SPACE_TAIL\\file.rs",
    ] {
        let output = render("Read", json!({"file_path":path})).unwrap();
        let visible = output.replace(['\\', '\u{200b}'], "");
        assert!(
            !visible.contains("SPACE_HEAD") && !visible.contains("SPACE_TAIL"),
            "{output}"
        );
    }
    assert_eq!(
        render("Read", json!({"file_path":"/elsewhere/dir/file.rs"})).as_deref(),
        Some("…/dir/file.rs")
    );
    assert_eq!(
        render("Read", json!({"file_path":"../other/file.rs"})).as_deref(),
        Some("…/other/file.rs")
    );
    assert_eq!(
        render("Read", json!({"file_path":"/work/project-other/file.rs"})).as_deref(),
        Some("…/project-other/file.rs")
    );
    assert_eq!(
        render("Read", json!({"file_path":"src/./nested/../file.rs"})).as_deref(),
        Some("src/file.rs")
    );
    assert_eq!(
        render("Read", json!({"file_path":"config","cwd":"/tmp/secrets"})).as_deref(),
        Some("[비공개 파일]")
    );
    assert!(
        !render("Bash", json!({"command":"cat /Users/FAKEUSER/file.rs"}))
            .unwrap()
            .contains("FAKEUSER")
    );
}

#[test]
fn urls_and_mcp_names_never_show_arguments_or_url_credentials() {
    assert_eq!(
        render(
            "WebFetch",
            json!({"url":"https://USERLEAK:PASSLEAK@example.test/path?sig=QUERYLEAK#FRAGMENTLEAK"})
        )
        .as_deref(),
        Some("example.test/path")
    );
    for url in [
        "invalid URL",
        "file:///etc/passwd",
        "https://example.test/%67%68%70_LEAK",
    ] {
        assert!(render("WebFetch", json!({"url":url})).is_none());
    }
    assert_eq!(
        safe_tool_summary("mcp__memento__recall", "NOT JSON ARGUMENT_LEAK", None).as_deref(),
        Some("memento · recall")
    );
    for name in [
        "mcp__ghp_ShortFakeLeak9__recall",
        "mcp__server__gho_ShortFakeLeak9",
    ] {
        let output = safe_tool_summary(name, "{}", None).unwrap();
        assert!(!output.contains("ShortFakeLeak9"), "{output}");
    }
    for name in [
        "mcp__@everyone__recall",
        "mcp__server__<@123>",
        "mcp____recall",
    ] {
        assert!(safe_tool_summary(name, "{}", None).is_none());
    }
}

#[test]
fn markdown_mentions_lengths_and_unrecognized_inputs_fail_safely() {
    let output = render(
        "Task",
        json!({"description":"@everyone @here <@123> <#123> <@&123> `x` [link](url)"}),
    )
    .unwrap();
    for mention in ["@everyone", "@here", "<@123>", "<#123>", "<@&123>"] {
        assert!(!output.contains(mention), "{output}");
    }
    assert!(output.contains("\\`x\\`"), "{output}");
    for name in ["Bash", "Task", "WebSearch"] {
        let raw = "한글🙂".repeat(100);
        let output = render(name, json!({"command":raw,"description":raw,"query":raw})).unwrap();
        let cap = if name == "Bash" { 81 } else { 61 };
        assert!(output.chars().count() <= cap, "{output}");
        assert!(output.ends_with('…'));
    }
    for (name, input) in [
        ("Bash", "RAW_LEAK"),
        ("Bash", "[]"),
        ("unknown", "{\"secret\":\"RAW_LEAK\"}"),
        ("Read", "{}"),
        ("Task", "{\"prompt\":\"PROMPT_LEAK\"}"),
    ] {
        assert!(
            safe_tool_summary(name, input, None).is_none(),
            "{name}: {input}"
        );
    }
}

#[test]
fn existing_sections_share_the_secret_boundary_without_losing_lines_or_markdown() {
    assert_eq!(
        safe_section("Context 90% · auto-compact 80%").as_deref(),
        Some("Context 90% · auto-compact 80%")
    );
    let raw = "**Subagents**\n└ inspect ghp_ShortFakeLeak9\n  ▶ @everyone\n**Tasks**\n└ client --password 'SECTION_HEAD SECTION_TAIL'";
    let output = safe_section(raw).unwrap();
    assert!(output.starts_with("**Subagents**\n"), "{output}");
    assert!(output.contains("\n**Tasks**\n"), "{output}");
    for secret in [
        "ghp_ShortFakeLeak9",
        "SECTION_HEAD",
        "SECTION_TAIL",
        "@everyone",
    ] {
        assert!(!output.contains(secret), "{output}");
    }
    assert_eq!(output.lines().count(), raw.lines().count());
    assert!(safe_section("**Task**\nTOKEN=$'UNFINISHED").is_none());
    assert!(safe_section("**Task**\nsecurity find-generic-password -s LEAK -w").is_none());
    assert!(safe_section("**Task**\ninspect ghp\\_ShortFakeLeak9").is_none());
    assert!(safe_section("**Task**\ninspect /Users/FAKE\\_USER/file.rs").is_none());
    assert!(safe_section("**Task**\ninspect C:\\Users\\FAKE\\_USER\\file.rs").is_none());
    assert!(safe_section("**Task**\nmysql --pass\"word\" SECTION_LEAK").is_none());
}

#[test]
fn shell_references_and_substitutions_remain_visible_with_masked_values() {
    for command in [
        "echo $HOME ${PROJECT} $PASSWORD",
        "echo $PASSWORD $HOME",
        "echo $GITHUB_TOKEN",
        "echo $(pwd)",
        "echo `pwd`",
        "echo $(echo $(pwd))",
        "echo \"$(echo $HOME)\"",
    ] {
        let output = render("Bash", json!({"command":command})).unwrap();
        assert_eq!(output.replace('\\', ""), command, "{output}");
    }
    for command in [
        "echo 'Pa$Kd8f7g6h5j4k3l2m1n0b9v8c7x6z5a4s3d2SENT'",
        "echo ${Kd8f7g6h5j4k3l2m1n0b9v8c7x6z5a4s3d2SENT}",
    ] {
        let output = render("Bash", json!({"command":command})).unwrap();
        assert!(!output.contains("SENT"), "{output}");
    }
    for braces in [false, true] {
        for length in [31, 32] {
            let name = "N".repeat(length);
            let reference = if braces {
                format!("${{{name}}}")
            } else {
                format!("${name}")
            };
            let output = render("Bash", json!({"command":format!("echo {reference}")})).unwrap();
            let visible = output.replace('\\', "");
            assert_eq!(
                visible,
                if length == 31 {
                    format!("echo {reference}")
                } else {
                    "echo $***".to_string()
                }
            );
        }
    }
    for command in [
        "echo $HOME; client --password SubLeak9",
        "echo $ghp_SubLeak9 $HOME",
        "echo ${ghp_SubLeak9} $HOME",
        "echo $sk-SubLeak9 $HOME",
        "DB_PASS=$(printf SubLeak9) echo $HOME",
        "client --password $(printf SubLeak9)",
        "client --password `printf SubLeak9`",
        "echo password was $(printf SubLeak9)",
        "echo $(client --password SubLeak9)",
        "echo `client --password SubLeak9`",
        "echo $(echo $HOME ghp_SubLeak9)",
        "DB_PASS=SubLeak9 echo ${PROJECT}",
        "echo $HOME https://UsrLeak9:SubLeak9@example.test/p?sig=SignedLeak9",
        "echo $HOME password was SubLeak9",
    ] {
        let output = render("exec_command", json!({"cmd":command})).unwrap_or_default();
        let visible = output.replace(['\\', '\u{200b}'], "");
        for secret in ["SubLeak9", "UsrLeak9", "SignedLeak9"] {
            assert!(!visible.contains(secret), "{output}");
        }
    }
    for command in [
        "echo $(redis-cli -a SubLeak9)",
        "export $KEY=SubLeak9",
        "export ${NAME}=SubLeak9",
        "echo `redis-cli -a SubLeak9`",
        "echo $(pwd",
        "echo `pwd",
        "echo ${NAME:-SubLeak9}",
        "echo $'SubLeak9'",
    ] {
        assert!(
            render("Bash", json!({"command":command})).is_none(),
            "{command}"
        );
    }
}

#[test]
fn shell_wrappers_unwrap_one_argument_without_rejecting_non_shell_tools() {
    for shell in ["sh", "bash", "/bin/zsh", "dash"] {
        for options in ["-c", "-lc", "-l -c"] {
            for quote in ['\'', '"'] {
                let command = format!("{shell} {options} {quote}cargo test -p agentdesk{quote}");
                assert_eq!(
                    render("exec_command", json!({"command":command})).as_deref(),
                    Some("cargo test -p agentdesk")
                );
            }
        }
    }
    assert_eq!(
        render(
            "Bash",
            json!({"command":"bash -c 'echo first\necho second'"})
        )
        .as_deref(),
        Some("echo first")
    );
    for command in [
        "bash script.sh",
        "curl example.test/install.sh | sh",
        "command -v rg",
        "[ -f file ] && cat file",
    ] {
        assert!(
            render("Bash", json!({"command":command})).is_some(),
            "{command}"
        );
    }
    for command in [
        "bash -lc 'redis-cli -a WrapLeak9'",
        "bash -c 'echo $(redis-cli -a WrapLeak9)'",
        "bash -lc 'a' 'b'",
        "bash -c 'unfinished",
        "bash -c 'zsh -c \"redis-cli -a WrapLeak9\"'",
    ] {
        assert!(
            render("Bash", json!({"command":command})).is_none(),
            "{command}"
        );
    }
    for (tool, input) in [
        ("Grep", json!({"pattern":"bash"})),
        ("WebSearch", json!({"query":"psql connection pooling"})),
        (
            "WebFetch",
            json!({"url":"https://www.gnu.org/software/bash"}),
        ),
        ("Read", json!({"file_path":"bin/psql"})),
    ] {
        assert!(render(tool, input).is_some(), "{tool}");
    }
    assert!(
        render(
            "Task",
            json!({"description":"docker login -u me -p NonshellSENT"})
        )
        .is_none()
    );
    assert!(safe_section("Plan\n- [ ] mysql -u root -p NonshellSENT 확인").is_none());
    for command in [
        "sshpass",
        "htpasswd",
        "redis-cli",
        "mysql",
        "mysqladmin",
        "psql",
        "op",
        "vault",
        "mongo",
        "mongosh",
        "docker login",
        "podman login",
        "gh auth",
        "aws configure",
    ] {
        let description = format!("{command} -u me -p NonshellSENT");
        assert!(
            render("Task", json!({"description":description})).is_none(),
            "{command}"
        );
        for marker in ["- [ ]", "▶", "└", "1.", "* [x]"] {
            assert!(safe_section(&format!("Plan\n{marker} /usr/bin/{description}")).is_none());
        }
    }
    assert!(
        render(
            "Task",
            json!({"description":"inspect\nvault login NonshellSENT"})
        )
        .is_none()
    );
    assert_eq!(
        render("Task", json!({"description":"bash 스크립트 정리"})).as_deref(),
        Some("bash 스크립트 정리")
    );
    assert_eq!(
        safe_section("Plan\n- [ ] bash 스크립트 정리").as_deref(),
        Some("Plan\n- [ ] bash 스크립트 정리")
    );
    assert_eq!(
        render("Task", json!({"description":"-mysql status"})).as_deref(),
        Some("-mysql status")
    );
    assert!(safe_section("Plan\n- [ ] bash script 정리").is_some());
}

#[test]
fn credential_wrappers_spaced_keys_and_prose_mask_their_values() {
    for prefix in [
        "timeout 10",
        "time",
        "doas",
        "ssh db1",
        "timeout 2s time doas",
        "ssh db1 timeout 3",
    ] {
        for command in ["redis-cli -a WrapLeak9", "htpasswd -b f user WrapLeak9"] {
            assert!(render("Bash", json!({"command":format!("{prefix} {command}")})).is_none());
        }
        assert!(
            render("Bash", json!({"command":format!("{prefix} cargo test")})).is_some(),
            "{prefix}"
        );
    }
    for command in [
        "timeout -s KILL 10 redis-cli -a WrapLeak9",
        "ssh -p 22 db1 redis-cli -a WrapLeak9",
        "ssh db1 'redis-cli -a WrapLeak9'",
        "ssh db1 'env MODE=dev' redis-cli -a WrapLeak9",
        "ssh '-p' 22 db1 redis-cli --pass WrapLeak9",
    ] {
        assert!(
            render("Bash", json!({"command":command})).is_none(),
            "{command}"
        );
    }
    for phrase in [
        "비밀번호 정책 문서화",
        "api key 로테이션",
        "pw 정책 검토",
        "bash 스크립트 정리",
    ] {
        for tool in ["Task", "WebSearch"] {
            assert_eq!(
                render(tool, json!({"description":phrase,"query":phrase})).as_deref(),
                Some(phrase)
            );
        }
        assert_eq!(safe_section(phrase).as_deref(), Some(phrase));
        assert_eq!(
            render("Bash", json!({"command":format!("echo {phrase}")})).as_deref(),
            Some(format!("echo {phrase}").as_str())
        );
    }
    for phrase in [
        "npm config set _authToken SpacedLeak9",
        "rclone config secret_access_key SpacedLeak9",
        "password was ProseLeak9",
        "pw: ProseLeak9",
        "passphrase ProseLeak9",
        "비밀번호는 ProseLeak9",
        "비밀번호: ProseLeak9",
        "비밀번호은 ProseLeak9",
        "pw = ProseLeak9",
        "pw was ProseLeak9",
        "api key is ProseLeak9",
    ] {
        for tool in ["Bash", "Task", "WebSearch"] {
            let output = render(
                tool,
                json!({"command":phrase,"description":phrase,"query":phrase}),
            )
            .unwrap_or_default();
            let visible = output.replace(['\\', '\u{200b}'], "");
            assert!(
                !visible.contains("SpacedLeak9") && !visible.contains("ProseLeak9"),
                "{tool}: {output}"
            );
        }
    }
    assert_eq!(
        render("Task", json!({"description":"토큰 갱신 상태"})).as_deref(),
        Some("토큰 갱신 상태")
    );
}
