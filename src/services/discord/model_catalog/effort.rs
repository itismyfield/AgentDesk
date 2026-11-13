//! Exact-model effort metadata from the account the next Codex launch uses.
use std::path::Path;

use serde::Deserialize;

pub(in crate::services::discord) const LEVELS: [&str; 6] =
    ["low", "medium", "high", "xhigh", "max", "ultra"];
const MAX_CACHE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
struct Cache {
    models: Vec<Model>,
}

#[derive(Deserialize)]
struct Model {
    slug: String,
    supported_reasoning_levels: Option<Vec<Effort>>,
}

#[derive(Deserialize)]
struct Effort {
    effort: String,
}

pub(in crate::services::discord) fn codex_supported_efforts(
    model: Option<&str>,
    codex_home: Option<&Path>,
) -> Result<Vec<String>, String> {
    let model = exact_model(model)?;
    let home = codex_home
        .map(Path::to_path_buf)
        .or_else(crate::services::codex_tui::rollout_tail::default_codex_home)
        .ok_or_else(|| "Codex launch home을 확인하지 못했습니다.".to_string())?;
    crate::config::test_instance::environment::validate_provider_home("codex", &home)?;
    let raw = super::bounded_cache_file::read_regular_file_bounded(
        &home.join("models_cache.json"),
        MAX_CACHE_BYTES,
    )
    .ok_or_else(|| "Codex 모델별 effort metadata를 읽지 못했습니다.".to_string())?;
    from_cache(&raw, model)
}

fn exact_model(model: Option<&str>) -> Result<&str, String> {
    model
        .map(str::trim)
        .filter(|model| !model.is_empty() && !model.eq_ignore_ascii_case("default"))
        .ok_or_else(|| "현재 채널 Codex 모델을 확인하지 못했습니다.".to_string())
}

fn from_cache(raw: &[u8], model: &str) -> Result<Vec<String>, String> {
    let cache: Cache = serde_json::from_slice(raw)
        .map_err(|_| "Codex effort metadata 형식을 확인하지 못했습니다.".to_string())?;
    let mut matches = cache.models.into_iter().filter(|entry| entry.slug == model);
    let matched = matches
        .next()
        .ok_or_else(|| "현재 Codex 모델의 effort metadata가 없습니다.".to_string())?;
    if matches.next().is_some() {
        return Err("현재 Codex 모델의 effort metadata가 중복됩니다.".into());
    }
    let entries = matched
        .supported_reasoning_levels
        .ok_or_else(|| "현재 Codex 모델의 지원 effort가 확인되지 않았습니다.".to_string())?;
    let levels: Vec<String> = LEVELS
        .into_iter()
        .filter(|level| entries.iter().any(|entry| entry.effort == *level))
        .map(str::to_string)
        .collect();
    if levels.is_empty() {
        return Err("현재 Codex 모델의 지원 effort가 확인되지 않았습니다.".into());
    }
    Ok(levels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(models: &[(&str, &[&str])]) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"models": models.iter().map(|(model, efforts)| {
            serde_json::json!({"slug":model,"supported_reasoning_levels":efforts.iter().map(|effort| serde_json::json!({"effort":effort})).collect::<Vec<_>>()})
        }).collect::<Vec<_>>()})).unwrap()
    }

    #[test]
    fn codex_effort_metadata_is_exact_model_specific_and_canonical() {
        let raw = metadata(&[
            (
                "gpt-6-astra",
                &["ultra", "high", "medium", "low", "xhigh", "max"],
            ),
            ("gpt-6-luna", &["low", "medium", "high", "xhigh", "max"]),
            ("gpt-5.5", &["low", "medium", "high", "xhigh"]),
        ]);
        assert_eq!(from_cache(&raw, "gpt-6-astra").unwrap(), LEVELS);
        assert_eq!(from_cache(&raw, "gpt-6-luna").unwrap(), LEVELS[..5]);
        assert_eq!(from_cache(&raw, "gpt-5.5").unwrap(), LEVELS[..4]);
        assert!(from_cache(&raw, "gpt-6-astra-preview").is_err());
    }

    #[test]
    fn unknown_or_ambiguous_codex_metadata_never_offers_guessed_efforts() {
        assert!(exact_model(None).is_err());
        assert!(exact_model(Some("default")).is_err());
        assert!(from_cache(br#"{"models":[{"slug":"gpt-6-astra"}]}"#, "gpt-6-astra").is_err());
        assert!(from_cache(&metadata(&[("gpt-6-astra", &[])]), "gpt-6-astra").is_err());
        assert!(
            from_cache(
                &metadata(&[("gpt-6-astra", &["ultra"]), ("gpt-6-astra", &["high"])]),
                "gpt-6-astra"
            )
            .is_err()
        );
        assert!(from_cache(b"not json", "gpt-6-astra").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_instance_effort_cache_rejects_foreign_explicit_and_default_home() {
        if !crate::services::provider_auth_profile::instance_env_tests::instance_child(concat!(
            module_path!(),
            "::test_instance_effort_cache_rejects_foreign_explicit_and_default_home"
        )) {
            return;
        }
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        crate::config::test_instance::initialize().unwrap();
        let instance = crate::config::test_instance::current().unwrap().unwrap();
        let home = instance.root.join("home/.adk/profiles/codex/test-codex");
        std::fs::write(
            home.join("models_cache.json"),
            metadata(&[("gpt-6-astra", &["high"])]),
        )
        .unwrap();
        let foreign = instance.root.join("foreign-codex");
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(
            foreign.join("models_cache.json"),
            metadata(&[("gpt-6-astra", &["ultra"])]),
        )
        .unwrap();
        assert_eq!(
            codex_supported_efforts(Some("gpt-6-astra"), Some(&home)).unwrap(),
            ["high"]
        );
        assert_eq!(
            codex_supported_efforts(Some("gpt-6-astra"), None).unwrap(),
            ["high"]
        );
        assert!(codex_supported_efforts(Some("gpt-6-astra"), Some(&foreign)).is_err());
        let _home = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "CODEX_HOME",
            &foreign,
        );
        assert!(codex_supported_efforts(Some("gpt-6-astra"), None).is_err());
    }

    #[test]
    fn named_codex_home_is_read_again_without_ambient_cache_fallback() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("models_cache.json");
        let read = || codex_supported_efforts(Some("gpt-6-astra"), Some(home.path()));
        assert!(read().is_err());
        std::fs::write(&path, metadata(&[("gpt-6-astra", &["high"])])).unwrap();
        assert_eq!(read().unwrap(), ["high"]);
        std::fs::write(&path, metadata(&[("gpt-6-astra", &["ultra"])])).unwrap();
        assert_eq!(read().unwrap(), ["ultra"]);
        std::fs::write(&path, b"broken cache").unwrap();
        assert!(read().is_err());
    }
}
