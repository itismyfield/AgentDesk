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
    /// Ask from a JSON body: store the request and post it to the category's Decision forum
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
    let root = crate::config::runtime_root().ok_or("decision: no runtime root")?;
    let (base, read) = (super::client::api_base(), super::client::LOCAL_POST_READ);
    let unanswered = |error| format!("decision: no answer from {base}: {error:?}");
    match args.command {
        DecisionCommand::Ask { json_file } => {
            let body = read_body(&json_file)?;
            let mut presented = None;
            let (status, reply) = ask(&root, &body, |capability| {
                presented = Some(capability.to_string());
                let owner = Some((HEADER, capability));
                super::client::post_local_with(&base, "/api/decisions", &body, read, owner)
                    .map_err(unanswered)
            })?;
            settle(&root, status, &reply, presented.as_deref(), print)
        }
        DecisionCommand::Get { id } => {
            let id = uuid::Uuid::parse_str(id.trim())
                .map_err(|_| "decision get: the id is not a request id".to_string())?;
            let (status, reply) = get(&root, id, |path, capability| {
                super::client::get_local_with(&base, path, read, (HEADER, capability))
                    .map_err(unanswered)
            })?;
            print(&reply);
            match status {
                200 => Ok(()),
                status => Err(format!("decision get: HTTP {status}")),
            }
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

/// An answered ask. A stored request is linked to its capability before its answer is shown, so
/// an id on stdout can always be read back; any other answer is shown as the error it is.
fn settle(
    root: &std::path::Path,
    status: u16,
    reply: &Value,
    capability: Option<&str>,
    show: impl FnOnce(&Value),
) -> Result<(), String> {
    if !matches!(status, 201 | 202) {
        show(reply);
        return Err(format!("decision ask: HTTP {status}"));
    }
    remember(root, reply, capability)?;
    show(reply);
    Ok(())
}

/// Links the request an ask's answer names to the capability it was asked under.
fn remember(root: &std::path::Path, reply: &Value, capability: Option<&str>) -> Result<(), String> {
    let id = reply["request_id"].as_str().map(uuid::Uuid::parse_str);
    match (id, capability) {
        (Some(Ok(id)), Some(capability)) => owner_cache::remember(root, id, capability),
        _ => Err("decision ask: the answer names no request".into()),
    }
}

/// Reads a request this host asked: its capability comes from the private cache and goes in the
/// request header, never on the command line or in the output.
fn get<T>(
    root: &std::path::Path,
    id: uuid::Uuid,
    send: impl FnOnce(&str, &str) -> Result<T, String>,
) -> Result<T, String> {
    let capability = owner_cache::of_request(root, id)?;
    send(&format!("/api/decisions/{id}"), &capability)
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
