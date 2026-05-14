//! Spoken voice command parsing and lobby routing helpers.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

use crate::config::{AgentDef, Config};
use crate::voice::barge_in::{BargeInSensitivity, parse_sensitivity_command};
use crate::voice::config::DEFAULT_ACTIVE_AGENT_TTL_SECS;
use crate::voice::progress;

pub(crate) const VOICE_ACTIVE_AGENT_CONTEXT_TTL: Duration =
    Duration::from_secs(DEFAULT_ACTIVE_AGENT_TTL_SECS);
pub(crate) const DEFAULT_WAKE_WORD: &str = "agentdesk";

static LANGUAGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?iu)(?:언어|language|lang)\s*(?:를|을|은|=|:|to|as)?\s*(한국어|한글|korean|ko|영어|english|en)\b|(?:한국어|한글|korean|ko|영어|english|en)\s*(?:로|으로)?\s*(?:말해|대답|전환|바꿔|변경)",
    )
    .expect("valid voice language regex")
});
static VOICE_CLONE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?iu)(?:voice\s*clone|보이스\s*클론|목소리\s*복제|음성\s*복제)")
        .expect("valid voice clone regex")
});
static VOICE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?iu)(?:목소리|음성|voice)\s*(?:를|을|은|=|:|to|as)?\s*([[:alnum:]가-힣_.:\- ]{2,80})",
    )
    .expect("valid voice selection regex")
});
static WAKE_WORD_SET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?iu)(?:호출어|웨이크\s*워드|wake\s*word)\s*(?:를|을|은|=|:|to|as)?\s*(.+)")
        .expect("valid wake word regex")
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VoiceCommand {
    Sensitivity(BargeInSensitivity),
    VerboseProgress(bool),
    Language(String),
    TtsVoice(String),
    VoiceClone { reference: Option<String> },
    WakeWords(WakeWordCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WakeWordCommand {
    EnableDefault,
    Disable,
    Set(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WakeWordMatch {
    pub(crate) wake_word: String,
    pub(crate) remaining: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WakeWordDecision {
    NotRequired(String),
    Matched(WakeWordMatch),
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VoiceAliasCollision {
    pub(crate) normalized: String,
    pub(crate) first_agent_id: String,
    pub(crate) first_alias: String,
    pub(crate) second_agent_id: String,
    pub(crate) second_alias: String,
}

impl std::fmt::Display for VoiceAliasCollision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "voice alias collision `{}`: `{}` ({}) conflicts with `{}` ({})",
            self.normalized,
            self.first_alias,
            self.first_agent_id,
            self.second_alias,
            self.second_agent_id
        )
    }
}

impl std::error::Error for VoiceAliasCollision {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VoiceAgentRoute {
    pub(crate) agent_id: String,
    pub(crate) channel_id: u64,
    pub(crate) provider: String,
    pub(crate) matched_alias: String,
    pub(crate) remaining_transcript: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VoiceActiveAgentContext {
    pub(crate) agent_id: String,
    pub(crate) channel_id: u64,
    pub(crate) updated_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VoiceLobbyRouteDecision {
    Routed(VoiceAgentRoute),
    ContinueActive {
        agent_id: String,
        channel_id: u64,
        transcript: String,
    },
    NeedAgent,
}

pub(crate) fn parse_voice_command(transcript: &str) -> Option<VoiceCommand> {
    let transcript = transcript.trim();
    if transcript.is_empty() {
        return None;
    }

    if let Some(command) = progress::parse_verbose_progress_command(transcript) {
        return Some(VoiceCommand::VerboseProgress(command.enabled()));
    }
    if let Some(sensitivity) = parse_sensitivity_command(transcript) {
        return Some(VoiceCommand::Sensitivity(sensitivity));
    }
    if let Some(command) = parse_wake_word_command(transcript) {
        return Some(VoiceCommand::WakeWords(command));
    }
    if let Some(language) = parse_language_command(transcript) {
        return Some(VoiceCommand::Language(language));
    }
    if let Some(command) = parse_voice_clone_command(transcript) {
        return Some(command);
    }
    if let Some(voice) = parse_tts_voice_command(transcript) {
        return Some(VoiceCommand::TtsVoice(voice));
    }

    None
}

pub(crate) fn wake_word_decision(
    transcript: &str,
    wake_words: &[String],
    required: bool,
) -> WakeWordDecision {
    let transcript = transcript.trim();
    if !required {
        return WakeWordDecision::NotRequired(transcript.to_string());
    }

    let first_token = transcript
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|ch: char| !ch.is_alphanumeric());
    let normalized_first = normalize_voice_alias_key(first_token);

    for wake_word in wake_words
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        let normalized_wake_word = normalize_voice_alias_key(wake_word);
        if normalized_wake_word.is_empty() {
            continue;
        }
        if normalized_first == normalized_wake_word {
            let remaining = transcript
                .split_once(char::is_whitespace)
                .map(|(_, rest)| rest.trim().to_string())
                .unwrap_or_default();
            return WakeWordDecision::Matched(WakeWordMatch {
                wake_word: wake_word.to_string(),
                remaining,
            });
        }
    }

    WakeWordDecision::Missing
}

pub(crate) fn normalize_voice_alias_key(value: &str) -> String {
    value
        .nfc()
        .flat_map(char::to_lowercase)
        .filter(|ch| ch.is_alphanumeric())
        .collect::<String>()
        .nfc()
        .collect()
}

pub(crate) fn validate_agent_alias_collisions(
    agents: &[AgentDef],
) -> Result<(), VoiceAliasCollision> {
    let mut seen: HashMap<String, (String, String)> = HashMap::new();
    for agent in agents {
        for alias in agent_voice_aliases(agent) {
            let normalized = normalize_voice_alias_key(&alias);
            if normalized.is_empty() {
                continue;
            }
            if let Some((first_agent_id, first_alias)) = seen.get(&normalized) {
                if first_agent_id != &agent.id {
                    return Err(VoiceAliasCollision {
                        normalized,
                        first_agent_id: first_agent_id.clone(),
                        first_alias: first_alias.clone(),
                        second_agent_id: agent.id.clone(),
                        second_alias: alias,
                    });
                }
                continue;
            }
            seen.insert(normalized, (agent.id.clone(), alias));
        }
    }
    Ok(())
}

pub(crate) fn resolve_voice_lobby_route(
    config: &Config,
    transcript: &str,
    active_context: Option<&VoiceActiveAgentContext>,
    now: Instant,
) -> Result<VoiceLobbyRouteDecision, VoiceAliasCollision> {
    validate_agent_alias_collisions(&config.agents)?;

    let transcript = transcript.trim();
    if transcript.is_empty() {
        return Ok(VoiceLobbyRouteDecision::NeedAgent);
    }

    let mut best_route: Option<(usize, VoiceAgentRoute)> = None;
    for agent in &config.agents {
        if !agent.voice_enabled {
            continue;
        }
        let Some((provider, channel_id)) = first_explicit_agent_channel(agent) else {
            continue;
        };
        let Some(route_transcript) = transcript_after_agent_wake_word(agent, transcript) else {
            continue;
        };
        for alias in agent_voice_aliases(agent) {
            if let Some(alias_match) = match_spoken_alias_prefix(&route_transcript, &alias) {
                let score = normalize_voice_alias_key(&alias_match.matched_alias).len();
                if best_route
                    .as_ref()
                    .is_some_and(|(best_score, _)| *best_score >= score)
                {
                    continue;
                }
                best_route = Some((
                    score,
                    VoiceAgentRoute {
                        agent_id: agent.id.clone(),
                        channel_id,
                        provider: provider.clone(),
                        matched_alias: alias_match.matched_alias,
                        remaining_transcript: alias_match.remaining_transcript,
                    },
                ));
            }
        }
    }
    if let Some((_, route)) = best_route {
        return Ok(VoiceLobbyRouteDecision::Routed(route));
    }

    if let Some(active_context) = active_context
        && now.duration_since(active_context.updated_at) <= config.voice.active_agent_context_ttl()
        && let Some(agent) = config
            .agents
            .iter()
            .find(|agent| agent.id == active_context.agent_id)
        && agent.voice_enabled
        && let Some(transcript) = transcript_after_agent_wake_word(agent, transcript)
    {
        return Ok(VoiceLobbyRouteDecision::ContinueActive {
            agent_id: active_context.agent_id.clone(),
            channel_id: active_context.channel_id,
            transcript,
        });
    }

    Ok(VoiceLobbyRouteDecision::NeedAgent)
}

fn parse_language_command(transcript: &str) -> Option<String> {
    let captures = LANGUAGE_RE.captures(transcript)?;
    for idx in 1..captures.len() {
        if let Some(value) = captures
            .get(idx)
            .and_then(|m| normalize_language(m.as_str()))
        {
            return Some(value);
        }
    }
    let normalized = normalize_voice_alias_key(transcript);
    if normalized.contains("한국어") || normalized.contains("한글") || normalized.contains("korean")
    {
        Some("ko".to_string())
    } else if normalized.contains("영어") || normalized.contains("english") {
        Some("en".to_string())
    } else {
        None
    }
}

fn parse_voice_clone_command(transcript: &str) -> Option<VoiceCommand> {
    if !VOICE_CLONE_RE.is_match(transcript) {
        return None;
    }
    let reference = transcript
        .split_once(':')
        .map(|(_, rest)| rest.trim().to_string())
        .filter(|value| !value.is_empty());
    Some(VoiceCommand::VoiceClone { reference })
}

fn parse_tts_voice_command(transcript: &str) -> Option<String> {
    if VOICE_CLONE_RE.is_match(transcript) {
        return None;
    }
    let captures = VOICE_RE.captures(transcript)?;
    let raw = captures.get(1)?.as_str();
    let cleaned = raw
        .trim()
        .trim_end_matches("로 바꿔")
        .trim_end_matches("으로 바꿔")
        .trim_end_matches("로 변경")
        .trim_end_matches("으로 변경")
        .trim_end_matches("바꿔")
        .trim_end_matches("변경")
        .trim()
        .trim_matches(|ch: char| ch == '"' || ch == '\'' || ch == '`')
        .to_string();
    if cleaned.is_empty() {
        return None;
    }
    // F9 (#2046): "음성 채널 들어가", "voice memo 켜", "목소리 안 들려" 같은
    // 평범한 발화가 TTS voice id 로 오인되던 false-positive 방지. Edge/Azure
    // TTS voice id 형식(예: "ko-KR-SunHiNeural", "en-US-AriaNeural")에 매치되는
    // ASCII 영어 패턴만 허용. 한글/공백 포함 자연어는 invalid 로 reject.
    if !is_tts_voice_id_shape(&cleaned) {
        return None;
    }
    Some(cleaned)
}

fn is_tts_voice_id_shape(value: &str) -> bool {
    // ko-KR-SunHiNeural, en-US-AriaNeural 같은 ASCII 형식만 허용.
    // 최소 두 개 이상의 hyphen 으로 구분된 토큰, 알파넘만, 길이 3..=80.
    if value.len() < 3 || value.len() > 80 {
        return false;
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return false;
    }
    value.matches('-').count() >= 2
}

fn parse_wake_word_command(transcript: &str) -> Option<WakeWordCommand> {
    let normalized = normalize_voice_alias_key(transcript);
    let mentions_wake_word = normalized.contains("호출어")
        || normalized.contains("웨이크워드")
        || normalized.contains("wakeword");
    if !mentions_wake_word {
        return None;
    }

    if normalized.contains("끄") || normalized.contains("해제") || normalized.contains("off") {
        return Some(WakeWordCommand::Disable);
    }
    if normalized.contains("켜") || normalized.contains("on") || normalized.contains("enable") {
        return Some(WakeWordCommand::EnableDefault);
    }

    let raw_value = WAKE_WORD_SET_RE
        .captures(transcript)
        .and_then(|captures| captures.get(1))
        .map(|m| m.as_str().trim())?;
    let wake_words = raw_value
        .split([',', '/', '|'])
        .map(|value| {
            value
                .trim()
                .trim_end_matches("로 설정")
                .trim_end_matches("으로 설정")
                .trim_end_matches("설정")
                .trim()
                .to_string()
        })
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    (!wake_words.is_empty()).then_some(WakeWordCommand::Set(wake_words))
}

fn normalize_language(value: &str) -> Option<String> {
    match normalize_voice_alias_key(value).as_str() {
        "한국어" | "한글" | "korean" | "ko" => Some("ko".to_string()),
        "영어" | "english" | "en" => Some("en".to_string()),
        _ => None,
    }
}

fn agent_voice_aliases(agent: &AgentDef) -> Vec<String> {
    let mut aliases = vec![agent.id.clone(), agent.name.clone()];
    if let Some(name_ko) = agent
        .name_ko
        .clone()
        .filter(|value| !value.trim().is_empty())
    {
        aliases.push(name_ko);
    }
    aliases.extend(agent.aliases.iter().cloned());
    aliases.extend(agent.keywords.iter().cloned());
    for (_, channel) in agent.channels.iter() {
        let Some(channel) = channel else {
            continue;
        };
        aliases.extend(channel.aliases());
    }
    aliases
}

fn transcript_after_agent_wake_word(agent: &AgentDef, transcript: &str) -> Option<String> {
    let Some(wake_word) = agent
        .wake_word
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Some(transcript.trim().to_string());
    };

    match_spoken_alias_prefix(transcript, wake_word).map(|matched| matched.remaining_transcript)
}

fn first_explicit_agent_channel(agent: &AgentDef) -> Option<(String, u64)> {
    for (provider, channel) in agent.channels.iter() {
        let Some(channel) = channel else {
            continue;
        };
        let Some(channel_id) = channel
            .channel_id()
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        return Some((provider.to_string(), channel_id));
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SpokenAliasPrefixMatch {
    matched_alias: String,
    remaining_transcript: String,
}

fn match_spoken_alias_prefix(transcript: &str, alias: &str) -> Option<SpokenAliasPrefixMatch> {
    let alias_key = normalize_voice_alias_key(alias);
    if alias_key.is_empty() {
        return None;
    }

    let accepted_keys = accepted_address_alias_keys(&alias_key);
    let normalized_transcript = transcript.nfc().collect::<String>();
    let transcript = normalized_transcript.trim_start();
    let mut last_prefix_key = String::new();
    for end in transcript
        .char_indices()
        .map(|(idx, ch)| idx + ch.len_utf8())
        .chain(std::iter::once(transcript.len()))
    {
        let prefix = &transcript[..end];
        let prefix_key = normalize_voice_alias_key(prefix);
        if prefix_key.is_empty() || prefix_key == last_prefix_key {
            continue;
        }
        last_prefix_key = prefix_key.clone();

        if accepted_keys.iter().any(|key| key == &prefix_key) {
            let remaining = &transcript[end..];
            if remaining
                .chars()
                .next()
                .is_some_and(|ch| ch.is_alphanumeric())
            {
                continue;
            }
            return Some(SpokenAliasPrefixMatch {
                matched_alias: clean_spoken_alias(prefix),
                remaining_transcript: trim_spoken_address_separator(remaining).to_string(),
            });
        }

        if !accepted_keys.iter().any(|key| key.starts_with(&prefix_key)) {
            break;
        }
    }

    None
}

fn accepted_address_alias_keys(alias_key: &str) -> [String; 4] {
    [
        alias_key.to_string(),
        format!("{alias_key}에게"),
        format!("{alias_key}야"),
        format!("{alias_key}아"),
    ]
}

fn clean_spoken_alias(value: &str) -> String {
    value
        .trim()
        .trim_matches(|ch: char| !ch.is_alphanumeric())
        .to_string()
}

fn trim_spoken_address_separator(value: &str) -> &str {
    value.trim_start_matches(|ch: char| ch.is_whitespace() || !ch.is_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentChannel, AgentChannelConfig, AgentChannels, AgentDef};

    fn agent(id: &str, name: &str, name_ko: Option<&str>, channel_id: &str) -> AgentDef {
        AgentDef {
            id: id.to_string(),
            name: name.to_string(),
            name_ko: name_ko.map(str::to_string),
            aliases: Vec::new(),
            wake_word: None,
            voice_enabled: true,
            sensitivity_mode: None,
            voice: crate::config::AgentVoiceConfig::default(),
            provider: "codex".to_string(),
            channels: AgentChannels {
                codex: Some(AgentChannel::Detailed(AgentChannelConfig {
                    id: Some(channel_id.to_string()),
                    aliases: vec![format!("{id}-alias")],
                    ..AgentChannelConfig::default()
                })),
                ..AgentChannels::default()
            },
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
        }
    }

    #[test]
    fn parses_voice_command_regex_cases() {
        let cases = [
            (
                "외부 보수 모드로 바꿔",
                VoiceCommand::Sensitivity(BargeInSensitivity::Conservative),
            ),
            (
                "기본 감도로 돌아가",
                VoiceCommand::Sensitivity(BargeInSensitivity::Normal),
            ),
            ("verbose on", VoiceCommand::VerboseProgress(true)),
            ("상세 진행 꺼", VoiceCommand::VerboseProgress(false)),
            ("언어 한국어", VoiceCommand::Language("ko".to_string())),
            ("language en", VoiceCommand::Language("en".to_string())),
            (
                "voice ko-KR-SunHiNeural",
                VoiceCommand::TtsVoice("ko-KR-SunHiNeural".to_string()),
            ),
            (
                "보이스 클론: /tmp/ref.wav",
                VoiceCommand::VoiceClone {
                    reference: Some("/tmp/ref.wav".to_string()),
                },
            ),
            (
                "호출어 에이디케이로 설정",
                VoiceCommand::WakeWords(WakeWordCommand::Set(vec!["에이디케이".to_string()])),
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(parse_voice_command(input), Some(expected), "input={input}");
        }
    }

    #[test]
    fn parse_voice_command_rejects_natural_phrases_as_voice_id() {
        // F9 (#2046): "음성 채널 들어가", "voice memo 켜" 같은 일상 발화에서
        // TtsVoice 가 false-positive 매치되던 회귀 방지. voice id 형식
        // (XX-XX-Neural)만 인정한다.
        assert_eq!(parse_voice_command("음성 채널 들어가"), None);
        assert_eq!(parse_voice_command("voice memo 켜"), None);
        assert_eq!(parse_voice_command("목소리 안 들려"), None);
        // 정상 TTS voice id 는 여전히 매치돼야 한다.
        assert_eq!(
            parse_voice_command("voice ko-KR-SunHiNeural"),
            Some(VoiceCommand::TtsVoice("ko-KR-SunHiNeural".to_string()))
        );
    }

    #[test]
    fn wake_word_required_strips_prefix() {
        let wake_words = vec!["에이디케이".to_string()];

        assert_eq!(
            wake_word_decision("에이디케이 상태 알려줘", &wake_words, true),
            WakeWordDecision::Matched(WakeWordMatch {
                wake_word: "에이디케이".to_string(),
                remaining: "상태 알려줘".to_string(),
            })
        );
        assert_eq!(
            wake_word_decision("상태 알려줘", &wake_words, true),
            WakeWordDecision::Missing
        );
    }

    #[test]
    fn alias_normalization_is_case_nfc_and_symbol_insensitive() {
        assert_eq!(normalize_voice_alias_key(" C D X! "), "cdx");
        assert_eq!(normalize_voice_alias_key("에이전트"), "에이전트");
    }

    #[test]
    fn lobby_alias_matching_normalizes_spoken_prefix_cases() {
        let mut td = agent("ch-td", "TD", Some("테크 디렉터"), "123");
        td.keywords = vec!["Tech Director".to_string(), "빌드담당".to_string()];
        let config = Config {
            agents: vec![td],
            ..Config::default()
        };

        let cases = [
            ("td 상태 알려줘", "상태 알려줘"),
            ("TD야 상태 알려줘", "상태 알려줘"),
            ("T D, 상태 알려줘", "상태 알려줘"),
            ("테크 디렉터, 빌드 어때?", "빌드 어때?"),
            ("테크 디렉터 빌드 어때?", "빌드 어때?"),
            ("tech-director 작업 시작", "작업 시작"),
            ("ch td alias 진행해", "진행해"),
            ("빌드담당에게 테스트는 통과해?", "테스트는 통과해?"),
        ];

        for (input, remaining) in cases {
            let routed = resolve_voice_lobby_route(&config, input, None, Instant::now())
                .unwrap_or_else(|_| panic!("route should not collide for input={input}"));
            match routed {
                VoiceLobbyRouteDecision::Routed(route) => {
                    assert_eq!(route.agent_id, "ch-td", "input={input}");
                    assert_eq!(route.channel_id, 123, "input={input}");
                    assert_eq!(route.remaining_transcript, remaining, "input={input}");
                }
                other => panic!("expected routed for input={input}, got {other:?}"),
            }
        }

        assert_eq!(
            resolve_voice_lobby_route(&config, "tdx 상태 알려줘", None, Instant::now()).unwrap(),
            VoiceLobbyRouteDecision::NeedAgent
        );
    }

    #[test]
    fn duplicate_aliases_across_agents_are_rejected() {
        let mut a = agent("agent-a", "Alpha", Some("알파"), "100");
        a.keywords.push("공통 별칭".to_string());
        let mut b = agent("agent-b", "Beta", Some("베타"), "200");
        b.keywords.push("공통별칭".to_string());

        let collision = validate_agent_alias_collisions(&[a, b]).unwrap_err();
        assert_eq!(collision.normalized, "공통별칭");
        assert_eq!(collision.first_agent_id, "agent-a");
        assert_eq!(collision.second_agent_id, "agent-b");
    }

    #[test]
    fn dashboard_aliases_are_part_of_lobby_resolution_and_collision_checks() {
        let mut td = agent("ch-td", "TD", Some("테크 디렉터"), "123");
        td.aliases.push("기술 책임자".to_string());
        let mut pd = agent("ch-pd", "PD", Some("프로덕트 디렉터"), "456");
        pd.aliases.push("프로덕트".to_string());
        let config = Config {
            agents: vec![td.clone(), pd],
            ..Config::default()
        };

        let routed =
            resolve_voice_lobby_route(&config, "기술책임자 상태 알려줘", None, Instant::now())
                .unwrap();
        match routed {
            VoiceLobbyRouteDecision::Routed(route) => {
                assert_eq!(route.agent_id, "ch-td");
                assert_eq!(route.remaining_transcript, "상태 알려줘");
            }
            other => panic!("expected routed, got {other:?}"),
        }

        let mut duplicate = td;
        duplicate.id = "other".to_string();
        duplicate.name = "Other".to_string();
        duplicate.name_ko = Some("다른 에이전트".to_string());
        let collision = validate_agent_alias_collisions(&[config.agents[0].clone(), duplicate])
            .expect_err("duplicate dashboard alias should collide");
        assert_eq!(collision.normalized, "기술책임자");
    }

    #[test]
    fn agent_wake_word_gates_lobby_alias_and_active_context() {
        let mut td = agent("ch-td", "TD", Some("테크 디렉터"), "123");
        td.wake_word = Some("헤이 데스크".to_string());
        let config = Config {
            agents: vec![td],
            ..Config::default()
        };
        let now = Instant::now();

        assert_eq!(
            resolve_voice_lobby_route(&config, "테크 디렉터 상태", None, now).unwrap(),
            VoiceLobbyRouteDecision::NeedAgent
        );

        let routed =
            resolve_voice_lobby_route(&config, "헤이 데스크 테크 디렉터 상태", None, now).unwrap();
        match routed {
            VoiceLobbyRouteDecision::Routed(route) => {
                assert_eq!(route.agent_id, "ch-td");
                assert_eq!(route.remaining_transcript, "상태");
            }
            other => panic!("expected routed, got {other:?}"),
        }

        let active = VoiceActiveAgentContext {
            agent_id: "ch-td".to_string(),
            channel_id: 123,
            updated_at: now,
        };
        assert_eq!(
            resolve_voice_lobby_route(&config, "상태 이어서", Some(&active), now).unwrap(),
            VoiceLobbyRouteDecision::NeedAgent
        );
        assert_eq!(
            resolve_voice_lobby_route(&config, "헤이 데스크 상태 이어서", Some(&active), now)
                .unwrap(),
            VoiceLobbyRouteDecision::ContinueActive {
                agent_id: "ch-td".to_string(),
                channel_id: 123,
                transcript: "상태 이어서".to_string(),
            }
        );
    }

    #[test]
    fn lobby_route_uses_first_token_alias_then_active_context() {
        let config = Config {
            agents: vec![agent(
                "project-agentdesk",
                "AgentDesk",
                Some("에이디케이"),
                "123",
            )],
            ..Config::default()
        };
        let now = Instant::now();

        let routed = resolve_voice_lobby_route(&config, "에이디케이 이슈 봐줘", None, now).unwrap();
        assert_eq!(
            routed,
            VoiceLobbyRouteDecision::Routed(VoiceAgentRoute {
                agent_id: "project-agentdesk".to_string(),
                channel_id: 123,
                provider: "codex".to_string(),
                matched_alias: "에이디케이".to_string(),
                remaining_transcript: "이슈 봐줘".to_string(),
            })
        );

        let active = VoiceActiveAgentContext {
            agent_id: "project-agentdesk".to_string(),
            channel_id: 123,
            updated_at: now - Duration::from_secs(60),
        };
        assert_eq!(
            resolve_voice_lobby_route(&config, "계속 진행해", Some(&active), now).unwrap(),
            VoiceLobbyRouteDecision::ContinueActive {
                agent_id: "project-agentdesk".to_string(),
                channel_id: 123,
                transcript: "계속 진행해".to_string(),
            }
        );
    }

    #[test]
    fn lobby_active_context_expires_after_configured_ttl() {
        let mut config = Config {
            agents: vec![agent(
                "project-agentdesk",
                "AgentDesk",
                Some("에이디케이"),
                "123",
            )],
            ..Config::default()
        };
        config.voice.active_agent_ttl_seconds = 60;
        let now = Instant::now();
        let active = VoiceActiveAgentContext {
            agent_id: "project-agentdesk".to_string(),
            channel_id: 123,
            updated_at: now - Duration::from_secs(61),
        };

        assert_eq!(
            resolve_voice_lobby_route(&config, "계속 진행해", Some(&active), now).unwrap(),
            VoiceLobbyRouteDecision::NeedAgent
        );
    }
}
