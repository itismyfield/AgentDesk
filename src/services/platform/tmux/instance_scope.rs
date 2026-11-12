use std::ffi::OsStr;
use std::io;
use std::process::Command;

use crate::config::test_instance;

pub(crate) fn input_command(command: &mut Command, args: &[&str]) -> io::Result<()> {
    test_instance::configure_tmux_command(command)?;
    validate_args(args.iter().map(OsStr::new))?;
    command.arg("-u").args(args);
    Ok(())
}

pub(crate) fn validate_target(target: &str) -> io::Result<()> {
    if test_instance::current()
        .map_err(io::Error::other)?
        .is_none()
    {
        return Ok(());
    }
    let name = target.strip_prefix('=').unwrap_or(target);
    let name = name.strip_suffix(':').unwrap_or(name);
    if name.contains([':', '.', '%', '$', '@']) {
        return Err(io::Error::other("test pane target has no receipt identity"));
    }
    test_instance::validate_tmux_target(name)
}

pub(super) fn validate_command(command: &Command) -> io::Result<()> {
    validate_args(command.get_args())
}

fn validate_args<'a>(args: impl Iterator<Item = &'a OsStr>) -> io::Result<()> {
    let Some(instance) = test_instance::current().map_err(io::Error::other)? else {
        return Ok(());
    };
    let mut args = args;
    let mut command_seen = false;
    while let Some(argument) = args.next() {
        if !command_seen {
            if argument == "-S" {
                if args.next() != Some(instance.tmux.socket.as_os_str()) {
                    return Err(io::Error::other("test tmux socket override refused"));
                }
                continue;
            }
            if ["-u", "-N", "-C", "-V"]
                .iter()
                .any(|flag| argument == *flag)
            {
                continue;
            }
            if argument.as_encoded_bytes().starts_with(b"-") {
                return Err(io::Error::other("unsupported test tmux global option"));
            }
            command_seen = true;
        }
        if argument == "--" {
            break;
        }
        if argument == "kill-server" || argument == ";" {
            return Err(io::Error::other("test tmux cleanup requires a receipt"));
        }
        let Some(argument) = argument.to_str() else {
            return Err(io::Error::other("unsupported test tmux argument"));
        };
        if let Some(target) = argument
            .strip_prefix("-t")
            .or_else(|| argument.strip_prefix("-s"))
        {
            let target = if target.is_empty() {
                args.next()
                    .and_then(OsStr::to_str)
                    .ok_or_else(|| io::Error::other("test tmux target missing"))?
            } else {
                target
            };
            validate_target(target)?;
        }
    }
    Ok(())
}

pub(super) fn hook_tmux_prefix() -> io::Result<String> {
    let mut command = Command::new("tmux");
    test_instance::configure_tmux_command(&mut command)?;
    if test_instance::current()
        .map_err(io::Error::other)?
        .is_none()
    {
        return Ok("tmux".into());
    }
    let mut words = vec!["env".into()];
    for (key, value) in command.get_envs() {
        if value.is_none() {
            words.extend(["-u".into(), super::sh_single_quote(&key.to_string_lossy())]);
        }
    }
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            words.push(super::sh_single_quote(&format!(
                "{}={}", key.to_string_lossy(), value.to_string_lossy()
            )));
        }
    }
    words.push("tmux".into());
    words.extend(
        command
            .get_args()
            .map(|arg| super::sh_single_quote(&arg.to_string_lossy())),
    );
    Ok(words.join(" "))
}

#[cfg(all(test, unix))]
#[path = "instance_scope/host_command_tests.rs"]
mod tests;
