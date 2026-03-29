use crate::services::provider::ProviderKind;
use crate::services::{claude, codex, gemini};

pub async fn execute_simple(provider: ProviderKind, prompt: String) -> Result<String, String> {
    match provider {
        ProviderKind::Claude => {
            tokio::task::spawn_blocking(move || claude::execute_command_simple(&prompt))
                .await
                .map_err(|e| format!("Task join error: {}", e))?
        }
        ProviderKind::Codex => {
            tokio::task::spawn_blocking(move || codex::execute_command_simple(&prompt))
                .await
                .map_err(|e| format!("Task join error: {}", e))?
        }
        ProviderKind::Gemini => {
            tokio::task::spawn_blocking(move || gemini::execute_command_simple(&prompt))
                .await
                .map_err(|e| format!("Task join error: {}", e))?
        }
        ProviderKind::Unsupported(name) => Err(format!("Provider '{}' is not installed", name)),
    }
}
