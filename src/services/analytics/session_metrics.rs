use super::dto::MachineStatusResponse;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::process::Command;

fn parse_machine_config(value: &str) -> Option<Vec<(String, String)>> {
    serde_json::from_str::<Vec<Value>>(value)
        .ok()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let name = m.get("name")?.as_str()?.to_string();
                    let host = m.get("host").and_then(|h| h.as_str()).unwrap_or_else(|| {
                        m.get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("localhost")
                    });
                    Some((name, format!("{host}.local")))
                })
                .collect()
        })
        .filter(|machines: &Vec<(String, String)>| !machines.is_empty())
}

fn default_machine_config() -> Vec<(String, String)> {
    let hostname = crate::services::platform::hostname_short();
    vec![(hostname.clone(), hostname)]
}

async fn load_machine_config_pg(pool: &PgPool) -> Option<Vec<(String, String)>> {
    sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1")
        .bind("machines")
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .and_then(|value| parse_machine_config(&value))
}

pub async fn load_machine_config(pg_pool: Option<&PgPool>) -> Vec<(String, String)> {
    if let Some(pool) = pg_pool {
        return load_machine_config_pg(pool)
            .await
            .unwrap_or_else(default_machine_config);
    }

    default_machine_config()
}

pub async fn machine_status(pg_pool: Option<&PgPool>) -> MachineStatusResponse {
    let machines_config = load_machine_config(pg_pool).await;

    let machines = tokio::task::spawn_blocking(move || {
        let mut results = Vec::new();
        for (name, host) in machines_config {
            let online = Command::new("ping")
                .args(["-c1", "-W2", &host])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            results.push(json!({"name": name, "online": online}));
        }
        results
    })
    .await
    .unwrap_or_default();

    MachineStatusResponse { machines }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_machine_config_uses_host_and_falls_back_to_name() {
        let machines = parse_machine_config(
            r#"[
                { "name": "mac-mini", "host": "mini-host" },
                { "name": "mac-book" }
            ]"#,
        )
        .expect("valid machine config");

        assert_eq!(
            machines,
            vec![
                ("mac-mini".to_string(), "mini-host.local".to_string()),
                ("mac-book".to_string(), "mac-book.local".to_string()),
            ]
        );
    }

    #[test]
    fn parse_machine_config_ignores_entries_without_names() {
        let machines = parse_machine_config(
            r#"[
                { "host": "unnamed-host" },
                { "name": "named-host" }
            ]"#,
        )
        .expect("valid machine config");

        assert_eq!(
            machines,
            vec![("named-host".to_string(), "named-host.local".to_string())]
        );
    }

    #[test]
    fn parse_machine_config_returns_none_for_empty_or_invalid_values() {
        assert!(parse_machine_config("[]").is_none());
        assert!(parse_machine_config("not json").is_none());
    }
}
