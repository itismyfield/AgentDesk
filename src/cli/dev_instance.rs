use std::io::{self, Read};
#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};

#[derive(Args)]
pub(crate) struct DevInstanceArgs {
    #[command(subcommand)]
    action: DevInstanceAction,
}

#[derive(Subcommand)]
enum DevInstanceAction {
    CredentialWrite {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        name: String,
    },
}

pub(crate) fn execute(args: DevInstanceArgs) -> Result<(), String> {
    match args.action {
        DevInstanceAction::CredentialWrite { root, name } => {
            credential_write(&root, &name, io::stdin().lock())
        }
    }
}

pub(crate) fn reject_argument_error() {
    if std::env::args()
        .skip(1)
        .find(|argument| !argument.starts_with('-'))
        .as_deref()
        == Some("dev-instance")
    {
        eprintln!("credential write rejected");
        std::process::exit(1);
    }
}

fn credential_write(root: &Path, name: &str, input: impl Read) -> Result<(), String> {
    write_credential(root, name, input).map_err(|_| "credential write rejected".to_string())
}

#[cfg(unix)]
fn write_credential(root: &Path, name: &str, input: impl Read) -> io::Result<()> {
    use crate::utils::secret_file::{
        SecretFilePolicy, private_directory::PrivateDirectory, write_secret_file_with_policy,
    };
    let rejected = || io::Error::other("invalid instance credential declaration");
    if !matches!(
        name,
        "claude_bot_token"
            | "codex_bot_token"
            | "driver_bot_token"
            | "announce_bot_token"
            | "notify_bot_token"
    ) || !root.is_absolute()
        || root
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(rejected());
    }
    let leaf = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(rejected)?;
    if uuid::Uuid::parse_str(leaf)
        .map_err(|_| rejected())?
        .to_string()
        != leaf
    {
        return Err(rejected());
    }
    let parts: Vec<_> = root
        .components()
        .filter_map(|part| part.as_os_str().to_str())
        .collect();
    if parts
        .windows(2)
        .any(|pair| pair[0].eq_ignore_ascii_case(".adk") && pair[1].eq_ignore_ascii_case("release"))
    {
        return Err(rejected());
    }
    PrivateDirectory::open(root).map_err(|_| rejected())?;
    let mut token = Vec::new();
    input.take(4098).read_to_end(&mut token)?;
    if token.last() == Some(&b'\n') {
        token.pop();
    }
    if token.is_empty()
        || token.len() > 4096
        || token.iter().any(|byte| matches!(*byte, b'\n' | b'\r' | 0))
        || token.first().is_some_and(u8::is_ascii_whitespace)
        || token.last().is_some_and(u8::is_ascii_whitespace)
    {
        return Err(rejected());
    }
    write_secret_file_with_policy(
        &root.join("credential").join(name),
        token,
        SecretFilePolicy::FreshPrivate,
    )
}

#[cfg(not(unix))]
fn write_credential(_root: &Path, _name: &str, _input: impl Read) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private credentials require Unix",
    ))
}

#[cfg(all(test, unix))]
mod tests;
