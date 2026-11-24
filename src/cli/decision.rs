//! `agentdesk decision`: ask the operator a Decision and read it back. The requester capability
//! is made and cached here before the request leaves; only its hash reaches the server's store.

use clap::{Args, Subcommand};
use serde_json::Value;

mod owner_cache;

use crate::services::discord::decisions::requester::HEADER;

#[derive(Args)]
#[command(about = "Ask the operator to decide, and read a Decision request")]
pub(crate) struct DecisionArgs {
    #[command(subcommand)]
    command: DecisionCommand,
}

#[derive(Subcommand)]
pub(crate) enum DecisionCommand {
    /// Store a Decision request from a JSON body; nothing is posted until publication is wired
    Ask {
        /// Path of the request JSON, or `-` for stdin
        #[arg(long)]
        json_file: String,
    },
    /// Read one Decision request
    Get {
        /// Request id the ask answered
        id: String,
    },
}

pub(crate) fn run(args: DecisionArgs) -> Result<(), String> {
    match args.command {
        DecisionCommand::Ask { json_file } => {
            let body = read_body(&json_file)?;
            let root = crate::config::runtime_root().ok_or("decision ask: no runtime root")?;
            let base = super::client::api_base();
            let (status, reply) = ask(&root, &body, |capability| {
                let (read, owner) = (super::client::LOCAL_POST_READ, Some((HEADER, capability)));
                super::client::post_local_with(&base, "/api/decisions", &body, read, owner)
                    .map_err(|error| format!("decision ask: no answer from {base}: {error:?}"))
            })?;
            print(&reply);
            match status {
                202 => Ok(()),
                status => Err(format!("decision ask: HTTP {status}")),
            }
        }
        DecisionCommand::Get { id } => {
            let id = uuid::Uuid::parse_str(id.trim())
                .map_err(|_| "decision get: the id is not a request id".to_string())?;
            print(&super::client::get_json(&format!("/api/decisions/{id}"))?);
            Ok(())
        }
    }
}

/// Caches the capability under the request's own scope and only then sends: a request that left
/// this host always has its capability on disk, and a retry of the same ask presents the same one.
fn ask<T>(
    root: &std::path::Path,
    body: &Value,
    send: impl FnOnce(&str) -> Result<T, String>,
) -> Result<T, String> {
    let scope = owner_cache::Scope::of(body)?;
    let capability = owner_cache::prepare(root, &scope)?;
    send(&capability)
}

fn read_body(path: &str) -> Result<Value, String> {
    let text = match path {
        "-" => std::io::read_to_string(std::io::stdin()),
        path => std::fs::read_to_string(path),
    };
    let text = text.map_err(|error| format!("decision ask: cannot read {path}: {error}"))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("decision ask: {path} is not JSON: {error}"))
}

fn print(reply: &Value) {
    let pretty = serde_json::to_string_pretty(reply).unwrap_or_else(|_| reply.to_string());
    println!("{pretty}");
}

#[cfg(test)]
#[path = "decision_tests.rs"]
mod tests;
