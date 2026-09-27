use super::*;
use std::cell::RefCell;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(windows)]
use std::os::windows::process::ExitStatusExt;
use std::rc::Rc;

#[test]
fn public_and_raw_exec_enforce_allowlist_before_runner() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let mut rejected: Vec<String> = [
        "tmux",
        "sh",
        "bash",
        "env",
        "/usr/bin/git",
        "/usr/bin/gh",
        "/bin/tmux",
        "GH",
        "Git",
        "GIT",
        "git ",
        " git",
        "git\t",
        "git\0",
        "./git",
        "../gh",
        "bin/git",
        "git-link",
        "git.exe",
        "C:\\bin\\git.exe",
        "",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    let fixture = tempfile::tempdir().unwrap();
    let git_path = fixture.path().join("git");
    std::fs::write(&git_path, "never execute this fixture").unwrap();
    rejected.push(git_path.to_string_lossy().into_owned());
    #[cfg(unix)]
    {
        let alias = fixture.path().join("git-link");
        std::os::unix::fs::symlink(&git_path, &alias).unwrap();
        rejected.push(alias.to_string_lossy().into_owned());
    }
    let runtime = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&runtime).unwrap();
    context.with(|ctx| {
        ctx.globals()
            .set("agentdesk", Object::new(ctx.clone()).unwrap())
            .unwrap();
        let recorded = Rc::clone(&calls);
        register_exec_ops_with_runner(&ctx, move |cmd, _path, args, timeout| {
            recorded
                .borrow_mut()
                .push((cmd.to_string(), args.to_vec(), timeout));
            Ok(Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: b"exec success sentinel\n".to_vec(),
                stderr: Vec::new(),
            })
        })
        .unwrap();
        for invocation in [
            r#"agentdesk.exec(command, ["--sentinel", "one arg"], {timeout_ms: 1234})"#,
            r#"agentdesk.__execRaw(command, '["--sentinel","one arg"]', 1234)"#,
        ] {
            for command in ["gh", "git"] {
                calls.borrow_mut().clear();
                ctx.globals().set("command", command).unwrap();
                let result: String = ctx.eval(invocation).unwrap();
                assert_eq!(result, "exec success sentinel", "{invocation}: {command}");
                assert_eq!(
                    *calls.borrow(),
                    vec![(
                        command.to_string(),
                        vec!["--sentinel".into(), "one arg".into()],
                        1234
                    )],
                    "{invocation}: {command}"
                );
            }
            for command in &rejected {
                calls.borrow_mut().clear();
                ctx.globals().set("command", command.as_str()).unwrap();
                let result: String = ctx.eval(invocation).unwrap();
                assert_eq!(
                    result,
                    format!("ERROR: command '{command}' not allowed"),
                    "{invocation}"
                );
                assert!(
                    calls.borrow().is_empty(),
                    "runner called by {invocation}: {command:?}"
                );
            }
        }
    });
}
