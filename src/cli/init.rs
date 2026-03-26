use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, Write as IoWrite};
use std::path::{Path, PathBuf};

use super::dcserver;

// ── Discord REST helpers ───────────────────────────────────────────

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

#[derive(serde::Deserialize)]
struct DiscordUser {
    username: String,
    id: String,
}

#[derive(serde::Deserialize)]
struct DiscordGuild {
    id: String,
    name: String,
}

#[derive(serde::Deserialize)]
struct DiscordChannel {
    id: String,
    name: Option<String>,
    #[serde(rename = "type")]
    channel_type: u8,
}

async fn discord_get<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    token: &str,
    path: &str,
) -> Result<T, String> {
    let url = format!("{}{}", DISCORD_API_BASE, path);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .map_err(|e| format!("HTTP error: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("Discord API {} — {}", resp.status(), path));
    }
    resp.json().await.map_err(|e| format!("Parse error: {}", e))
}

// ── Interactive helpers ────────────────────────────────────────────

fn prompt_line(msg: &str) -> String {
    print!("{}", msg);
    if let Err(e) = io::stdout().flush() {
        eprintln!("stdout flush failed: {e}");
        std::process::exit(1);
    }
    let mut buf = String::new();
    if let Err(e) = io::stdin().lock().read_line(&mut buf) {
        eprintln!("stdin read failed: {e}");
        std::process::exit(1);
    }
    buf.trim().to_string()
}

fn prompt_secret(msg: &str) -> String {
    // Simple secret prompt (no echo hiding — terminal may not support it)
    prompt_line(msg)
}

fn prompt_select(msg: &str, options: &[&str]) -> usize {
    println!("\n{}", msg);
    for (i, opt) in options.iter().enumerate() {
        println!("  [{}] {}", i + 1, opt);
    }
    loop {
        let input = prompt_line("선택: ");
        if let Ok(n) = input.parse::<usize>() {
            if n >= 1 && n <= options.len() {
                return n - 1;
            }
        }
        println!("1-{} 사이의 숫자를 입력하세요.", options.len());
    }
}

fn prompt_multi_select(msg: &str, options: &[(String, String)]) -> Vec<usize> {
    println!("\n{}", msg);
    for (i, (name, id)) in options.iter().enumerate() {
        println!("  [{}] {} ({})", i + 1, name, id);
    }
    println!("  (쉼표로 구분하여 여러 개 선택 가능, 예: 1,3,5)");
    loop {
        let input = prompt_line("선택: ");
        let selected: Vec<usize> = input
            .split(',')
            .filter_map(|s| s.trim().parse::<usize>().ok())
            .filter(|&n| n >= 1 && n <= options.len())
            .map(|n| n - 1)
            .collect();
        if !selected.is_empty() {
            return selected;
        }
        println!("최소 하나 이상 선택하세요.");
    }
}

// ── Template definitions ───────────────────────────────────────────

fn solo_org_yaml(channels: &[(String, String, String)]) -> String {
    // channels: Vec<(channel_id, channel_name, role_id)>
    let mut yaml = String::from(
        r#"version: 1
name: "My Agent Org"

prompts_root: "prompts"
skills_root: "skills"

agents:
  assistant:
    display_name: "Assistant"
    keywords: ["help", "assist"]

channels:
  by_id:
"#,
    );
    for (ch_id, _ch_name, _role) in channels {
        yaml.push_str(&format!("    \"{}\":\n      agent: assistant\n", ch_id));
    }
    yaml
}

fn small_team_org_yaml(channels: &[(String, String, String)]) -> String {
    let mut agents: HashMap<&str, bool> = HashMap::new();
    for (_, _, role) in channels {
        agents.insert(role, true);
    }

    let mut yaml = String::from(
        r#"version: 1
name: "Small Team Org"

prompts_root: "prompts"
skills_root: "skills"

agents:
"#,
    );
    for role in agents.keys() {
        yaml.push_str(&format!(
            "  {}:\n    display_name: \"{}\"\n    keywords: []\n",
            role,
            role.replace('-', " ")
        ));
    }
    yaml.push_str("\nchannels:\n  by_id:\n");
    for (ch_id, _ch_name, role) in channels {
        yaml.push_str(&format!("    \"{}\":\n      agent: {}\n", ch_id, role));
    }
    yaml
}

fn default_shared_prompt() -> &'static str {
    r#"# Shared Agent Rules

## Communication
- Respond in the user's language.
- Be concise and direct.

## Work Style
- Plan before implementing.
- Verify your work before reporting done.
- Fix bugs autonomously without asking "how should I fix this?"
"#
}

fn default_agent_prompt(role_id: &str) -> String {
    format!(
        r#"# {}

## identity
- role: {}
- mission: Assist with tasks in this channel

## working_rules
- Follow the shared agent rules
- Ask clarifying questions only when requirements are genuinely ambiguous

## response_contract
- Be concise and actionable
"#,
        role_id, role_id
    )
}

// ── Launchd plist ──────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn generate_launchd_plist(home: &Path, agentdesk_bin: &Path) -> String {
    let home_str = home.display();
    let bin_str = agentdesk_bin.display();
    let label = dcserver::AGENTDESK_DCSERVER_LAUNCHD_LABEL;
    // Use AGENTDESK_ROOT_DIR if set, otherwise default to ~/.adk/release
    let root_dir =
        dcserver::agentdesk_runtime_root().unwrap_or_else(|| home.join(".adk").join("release"));
    let root_str = root_dir.display();
    let logs_dir = root_dir.join("logs");
    let logs_str = logs_dir.display();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{bin_str}</string>
    <string>dcserver</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>5</integer>
  <key>WorkingDirectory</key>
  <string>{home_str}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{home_str}/.cargo/bin</string>
    <key>HOME</key>
    <string>{home_str}</string>
    <key>AGENTDESK_ROOT_DIR</key>
    <string>{root_str}</string>
  </dict>
  <key>StandardOutPath</key>
  <string>{logs_str}/dcserver.stdout.log</string>
  <key>StandardErrorPath</key>
  <string>{logs_str}/dcserver.stderr.log</string>
</dict>
</plist>"#
    )
}

// ── Bot settings generation ────────────────────────────────────────

/// Merge new bot entry into existing bot_settings.json content.
/// Preserves suffix_map, other bot entries, and custom fields.
fn generate_bot_settings(
    existing_path: &Path,
    token: &str,
    provider: &str,
    owner_id: Option<&str>,
) -> Result<String, String> {
    let token_hash = crate::services::discord::settings::discord_token_hash(token);
    let mut entry = serde_json::json!({
        "token": token,
        "provider": provider,
    });
    if let Some(oid) = owner_id {
        entry["owner_user_id"] = serde_json::Value::String(oid.into());
    }

    // Read existing file and merge, preserving all other keys
    let mut root: serde_json::Value = if existing_path.exists() {
        let content = fs::read_to_string(existing_path)
            .map_err(|e| format!("Failed to read {}: {e}", existing_path.display()))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse {}: {e}", existing_path.display()))?
    } else {
        serde_json::json!({})
    };

    if let Some(obj) = root.as_object_mut() {
        obj.insert(token_hash, entry);
    }

    serde_json::to_string_pretty(&root).map_err(|e| format!("JSON serialization failed: {e}"))
}

// ── Main init flow ─────────────────────────────────────────────────

pub fn handle_init(reconfigure: bool) {
    let root = dcserver::agentdesk_runtime_root().unwrap_or_else(|| {
        eprintln!("Error: cannot determine runtime directory");
        std::process::exit(1);
    });

    if !reconfigure && root.join("config").join("bot_settings.json").exists() {
        println!("기존 설정이 발견되었습니다: {}", root.display());
        println!("재설정하려면 reconfigure를 사용하세요.");
        return;
    }

    println!("═══════════════════════════════════════");
    println!("  AgentDesk 초기 설정 (v{})", super::VERSION);
    println!("═══════════════════════════════════════\n");

    if reconfigure {
        println!("[재설정 모드] 기존 설정을 보존하며 변경합니다.\n");
    }

    // Step 1: Bot token
    println!("Step 1/5: Discord 봇 토큰");
    println!("  Discord Developer Portal에서 봇을 생성하세요:");
    println!("  https://discord.com/developers/applications\n");

    let token = prompt_secret("봇 토큰 입력: ");
    if token.is_empty() {
        eprintln!("토큰이 비어있습니다. 종료합니다.");
        return;
    }

    // Validate token & fetch bot info
    println!("\n봇 정보를 확인 중...");
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Failed to create async runtime: {}", e);
            return;
        }
    };
    let client = reqwest::Client::new();

    let bot_user: DiscordUser = match rt.block_on(discord_get(&client, &token, "/users/@me")) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("토큰 검증 실패: {}", e);
            eprintln!("올바른 봇 토큰을 입력했는지 확인하세요.");
            return;
        }
    };
    println!("  봇 이름: {} (ID: {})", bot_user.username, bot_user.id);

    // Step 2: Fetch guilds + channels
    println!("\nStep 2/5: 서버 및 채널 스캔");
    let guilds: Vec<DiscordGuild> =
        match rt.block_on(discord_get(&client, &token, "/users/@me/guilds")) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("서버 목록 조회 실패: {}", e);
                return;
            }
        };

    if guilds.is_empty() {
        eprintln!("봇이 참여한 서버가 없습니다.");
        eprintln!("먼저 봇을 서버에 초대하세요.");
        return;
    }

    // Select guild
    let guild_names: Vec<&str> = guilds.iter().map(|g| g.name.as_str()).collect();
    let guild_idx = if guilds.len() == 1 {
        println!("  서버: {}", guilds[0].name);
        0
    } else {
        prompt_select("사용할 서버를 선택하세요:", &guild_names)
    };
    let guild = &guilds[guild_idx];

    // Fetch text channels
    let channels: Vec<DiscordChannel> = match rt.block_on(discord_get(
        &client,
        &token,
        &format!("/guilds/{}/channels", guild.id),
    )) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("채널 목록 조회 실패: {}", e);
            return;
        }
    };

    // Filter text channels (type 0 = text)
    let text_channels: Vec<(String, String)> = channels
        .into_iter()
        .filter(|c| c.channel_type == 0)
        .map(|c| (c.name.unwrap_or_else(|| c.id.clone()), c.id))
        .collect();

    if text_channels.is_empty() {
        eprintln!("텍스트 채널을 찾을 수 없습니다.");
        return;
    }

    // Select channels for agents
    let selected = prompt_multi_select("에이전트를 배정할 채널을 선택하세요:", &text_channels);

    // Step 3: Template selection + role assignment
    println!("\nStep 3/5: 템플릿 선택");
    let template_idx = prompt_select(
        "조직 템플릿을 선택하세요:",
        &[
            "solo — 단일 에이전트 (모든 채널 동일)",
            "small-team — 채널별 역할 분리",
        ],
    );

    let mut channel_mappings: Vec<(String, String, String)> = Vec::new(); // (id, name, role)

    match template_idx {
        0 => {
            // Solo: all channels get "assistant"
            for &idx in &selected {
                let (name, id) = &text_channels[idx];
                channel_mappings.push((id.clone(), name.clone(), "assistant".into()));
            }
        }
        1 => {
            // Small team: assign role per channel
            println!("\n각 채널에 역할 ID를 지정하세요 (예: td, pd, designer):");
            for &idx in &selected {
                let (name, id) = &text_channels[idx];
                let role = prompt_line(&format!("  #{} → 역할: ", name));
                let role = if role.is_empty() { name.clone() } else { role };
                channel_mappings.push((id.clone(), name.clone(), role));
            }
        }
        _ => unreachable!(),
    }

    // Provider selection
    let provider_idx = prompt_select(
        "AI 프로바이더를 선택하세요:",
        &["claude (Anthropic)", "codex (OpenAI)"],
    );
    let provider = match provider_idx {
        0 => "claude",
        1 => "codex",
        _ => "claude",
    };

    // Owner user ID (optional)
    println!("\nStep 4/5: 소유자 설정");
    let owner_input =
        prompt_line("Discord 사용자 ID (Enter로 건너뛰기 — 첫 메시지 발신자가 자동 등록): ");
    let owner_id = if owner_input.is_empty() {
        None
    } else {
        Some(owner_input.as_str())
    };

    // Generate configs
    println!("\nStep 5/5: 설정 파일 생성\n");
    if let Err(e) = fs::create_dir_all(&root) {
        eprintln!("Failed to create directory {}: {}", root.display(), e);
        return;
    }
    let config_dir = root.join("config");
    if let Err(e) = fs::create_dir_all(&config_dir) {
        eprintln!("Failed to create directory {}: {}", config_dir.display(), e);
        return;
    }

    // org.yaml — fresh install uses template, reconfigure preserves existing
    let org_path = config_dir.join("org.yaml");
    let org_yaml = if reconfigure && org_path.exists() {
        // Preserve existing org.yaml, only update channels.by_id entries
        let mut existing = fs::read_to_string(&org_path).unwrap_or_default();
        // Append new channel mappings that aren't already present
        for (ch_id, _ch_name, role) in &channel_mappings {
            let marker = format!("\"{}\":", ch_id);
            if !existing.contains(&marker) {
                let entry = format!("    \"{}\":\n      agent: {}\n", ch_id, role);
                if let Some(pos) = existing.find("  by_id:") {
                    let insert_at = existing[pos..]
                        .find('\n')
                        .map(|n| pos + n + 1)
                        .unwrap_or(existing.len());
                    existing.insert_str(insert_at, &entry);
                }
            }
        }
        existing
    } else {
        match template_idx {
            0 => solo_org_yaml(&channel_mappings),
            _ => small_team_org_yaml(&channel_mappings),
        }
    };
    if let Err(e) = write_with_backup(&org_path, &org_yaml, reconfigure) {
        eprintln!("Failed to write {}: {}", org_path.display(), e);
        return;
    }
    println!("  [OK] {}", org_path.display());

    // bot_settings.json
    let bs_path = config_dir.join("bot_settings.json");
    let bot_settings = match generate_bot_settings(&bs_path, &token, provider, owner_id) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bot_settings.json 생성 실패: {}", e);
            return;
        }
    };
    if let Err(e) = write_with_backup(&bs_path, &bot_settings, reconfigure) {
        eprintln!("Failed to write {}: {}", bs_path.display(), e);
        return;
    }
    println!("  [OK] {}", bs_path.display());

    // Create prompts
    let prompts_root = root.join("prompts");
    if let Err(e) = fs::create_dir_all(prompts_root.join("agents")) {
        eprintln!("Failed to create prompts/agents directory: {}", e);
        return;
    }

    let shared_path = prompts_root.join("_shared.md");
    if !shared_path.exists() {
        if let Err(e) = fs::write(&shared_path, default_shared_prompt()) {
            eprintln!("Failed to write {}: {}", shared_path.display(), e);
            return;
        }
        println!("  [OK] {}", shared_path.display());
    }

    let mut created_roles: Vec<String> = Vec::new();
    for (_, _, role) in &channel_mappings {
        if created_roles.contains(role) {
            continue;
        }
        let role_dir = prompts_root.join("agents").join(role);
        if let Err(e) = fs::create_dir_all(&role_dir) {
            eprintln!("Failed to create directory {}: {}", role_dir.display(), e);
            return;
        }
        let identity_path = role_dir.join("IDENTITY.md");
        if !identity_path.exists() {
            if let Err(e) = fs::write(&identity_path, default_agent_prompt(role)) {
                eprintln!("Failed to write {}: {}", identity_path.display(), e);
                return;
            }
            println!("  [OK] {}", identity_path.display());
        }
        created_roles.push(role.clone());
    }

    // Create skills/memory dirs
    for dir_name in ["skills", "memory"] {
        if let Err(e) = fs::create_dir_all(root.join(dir_name)) {
            eprintln!("Failed to create {} directory: {}", dir_name, e);
            return;
        }
    }

    // Binary setup + platform-specific service installation
    {
        let Some(home) = dirs::home_dir() else {
            eprintln!("Error: cannot determine home directory");
            return;
        };
        let agentdesk_bin = root.join("bin").join("agentdesk");

        // Create wrapper bin dir
        let bin_dir = root.join("bin");
        if let Err(e) = fs::create_dir_all(&bin_dir) {
            eprintln!("Failed to create directory {}: {}", bin_dir.display(), e);
            return;
        }

        // If no binary installed yet, copy current executable
        if !agentdesk_bin.exists() {
            if let Ok(current_exe) = std::env::current_exe() {
                if let Err(e) = fs::copy(&current_exe, &agentdesk_bin) {
                    eprintln!("  [WARN] 바이너리 복사 실패: {} — 수동으로 복사하세요", e);
                } else {
                    println!("  [OK] {}", agentdesk_bin.display());
                }
            }
        }

        // Platform-specific service installation (auto-detected)
        if let Err(e) = install_service(&home, &agentdesk_bin, reconfigure) {
            eprintln!("서비스 등록 실패: {e}");
            return;
        }

        println!("\n═══════════════════════════════════════");
        println!("  초기 설정 완료!");
        println!("═══════════════════════════════════════");
        println!("\n생성된 파일:");
        println!("  {} (org.yaml)", config_dir.join("org.yaml").display());
        println!(
            "  {} (bot_settings.json)",
            config_dir.join("bot_settings.json").display()
        );
        println!("  {} (prompts)", root.join("prompts").display());
        println!("\n다음 단계:");
        println!("  1. 프롬프트 파일을 편집하여 에이전트 성격을 정의하세요");
        println!("  2. Discord에서 봇에게 메시지를 보내 동작을 확인하세요");
    }
}

#[cfg(target_os = "macos")]
fn install_service(home: &Path, agentdesk_bin: &Path, reconfigure: bool) -> Result<(), String> {
    let plist_content = generate_launchd_plist(home, agentdesk_bin);
    let launch_agents = home.join("Library").join("LaunchAgents");
    fs::create_dir_all(&launch_agents)
        .map_err(|e| format!("Failed to create LaunchAgents directory: {e}"))?;
    let plist_filename = format!("{}.plist", dcserver::AGENTDESK_DCSERVER_LAUNCHD_LABEL);
    let plist_path = launch_agents.join(&plist_filename);
    write_with_backup(&plist_path, &plist_content, reconfigure)
        .map_err(|e| format!("Failed to write plist {}: {e}", plist_path.display()))?;
    println!("  [OK] {}", plist_path.display());

    let load_answer = prompt_line("\ndcserver를 지금 시작할까요? (Y/n): ");
    if load_answer.is_empty() || load_answer.to_lowercase().starts_with('y') {
        let label = dcserver::AGENTDESK_DCSERVER_LAUNCHD_LABEL;
        let uid = get_uid().map_err(|e| {
            format!("UID를 가져올 수 없습니다: {e} — 수동으로 launchctl을 실행하세요")
        })?;
        if dcserver::is_launchd_job_loaded(label) {
            let _ = std::process::Command::new("launchctl")
                .args([
                    "bootout",
                    &format!("gui/{}", uid),
                    &plist_path.to_string_lossy().to_string(),
                ])
                .status();
        }
        let status = std::process::Command::new("launchctl")
            .args([
                "bootstrap",
                &format!("gui/{}", uid),
                &plist_path.to_string_lossy().to_string(),
            ])
            .status();
        match status {
            Ok(s) if s.success() => println!("  [OK] dcserver 시작됨"),
            _ => println!(
                "  [WARN] launchd 등록 실패 — 수동으로 실행: launchctl bootstrap gui/$(id -u) {}",
                plist_path.display()
            ),
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_service(home: &Path, agentdesk_bin: &Path, _reconfigure: bool) -> Result<(), String> {
    let service_name = "agentdesk-dcserver";
    let root_dir =
        dcserver::agentdesk_runtime_root().unwrap_or_else(|| home.join(".adk").join("release"));
    let logs_dir = root_dir.join("logs");
    fs::create_dir_all(&logs_dir).map_err(|e| format!("Failed to create logs directory: {e}"))?;
    let unit_content = format!(
        "[Unit]\n\
         Description=AgentDesk Discord Control Server\n\
         After=network.target\n\n\
         [Service]\n\
         Type=simple\n\
         ExecStart={bin} dcserver\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         Environment=AGENTDESK_ROOT_DIR={root}\n\
         StandardOutput=append:{logs}/dcserver.stdout.log\n\
         StandardError=append:{logs}/dcserver.stderr.log\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        bin = agentdesk_bin.display(),
        root = root_dir.display(),
        logs = logs_dir.display()
    );

    let user_systemd = home.join(".config").join("systemd").join("user");
    fs::create_dir_all(&user_systemd)
        .map_err(|e| format!("Failed to create systemd user directory: {e}"))?;
    let unit_path = user_systemd.join(format!("{service_name}.service"));
    fs::write(&unit_path, &unit_content)
        .map_err(|e| format!("Failed to write systemd unit {}: {e}", unit_path.display()))?;
    println!("  [OK] {}", unit_path.display());

    let load_answer = prompt_line("\ndcserver를 지금 시작할까요? (Y/n): ");
    if load_answer.is_empty() || load_answer.to_lowercase().starts_with('y') {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        let status = std::process::Command::new("systemctl")
            .args(["--user", "enable", "--now", service_name])
            .status();
        match status {
            Ok(s) if s.success() => println!("  [OK] dcserver 시작됨 (systemd)"),
            _ => println!(
                "  [WARN] systemd 등록 실패 — 수동: systemctl --user enable --now {service_name}"
            ),
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn install_service(_home: &Path, agentdesk_bin: &Path, _reconfigure: bool) -> Result<(), String> {
    let service_name = "AgentDeskDcserver";
    let root_dir = dcserver::agentdesk_runtime_root().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".adk")
            .join("release")
    });
    let logs_dir = root_dir.join("logs");
    fs::create_dir_all(&logs_dir).map_err(|e| format!("Failed to create logs directory: {e}"))?;

    println!("  Windows 서비스 등록:");
    println!("  NSSM 사용 시:");
    println!(
        "    nssm install {service_name} \"{}\" dcserver",
        agentdesk_bin.display()
    );
    println!(
        "    nssm set {service_name} AppStdout \"{}\"",
        logs_dir.join("dcserver.stdout.log").display()
    );
    println!(
        "    nssm set {service_name} AppStderr \"{}\"",
        logs_dir.join("dcserver.stderr.log").display()
    );
    println!("    nssm start {service_name}");
    println!("  sc.exe 사용 시:");
    println!(
        "    sc create {service_name} binPath=\"{} dcserver\" start=auto",
        agentdesk_bin.display()
    );
    println!("    sc start {service_name}");

    let load_answer = prompt_line("\nNSSM으로 지금 등록할까요? (y/N): ");
    if load_answer.to_lowercase().starts_with('y') {
        let status = std::process::Command::new("nssm")
            .args([
                "install",
                service_name,
                &agentdesk_bin.to_string_lossy(),
                "dcserver",
            ])
            .status();
        match status {
            Ok(s) if s.success() => {
                // Configure NSSM log routing
                let stdout_log = logs_dir.join("dcserver.stdout.log");
                let stderr_log = logs_dir.join("dcserver.stderr.log");
                let _ = std::process::Command::new("nssm")
                    .args([
                        "set",
                        service_name,
                        "AppStdout",
                        &stdout_log.to_string_lossy(),
                    ])
                    .status();
                let _ = std::process::Command::new("nssm")
                    .args([
                        "set",
                        service_name,
                        "AppStderr",
                        &stderr_log.to_string_lossy(),
                    ])
                    .status();
                let _ = std::process::Command::new("nssm")
                    .args(["start", service_name])
                    .status();
                println!("  [OK] dcserver 시작됨 (NSSM)");
            }
            _ => println!("  [WARN] NSSM 등록 실패 — nssm이 설치되어 있는지 확인하세요"),
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn install_service(_home: &Path, agentdesk_bin: &Path, _reconfigure: bool) -> Result<(), String> {
    println!("  이 플랫폼에서는 자동 서비스 등록이 지원되지 않습니다.");
    println!("  수동으로 실행: {} dcserver", agentdesk_bin.display());
    Ok(())
}

fn write_with_backup(path: &Path, content: &str, reconfigure: bool) -> Result<(), io::Error> {
    if reconfigure && path.exists() {
        let existing = fs::read_to_string(path).unwrap_or_default();
        if existing == content {
            return Ok(()); // No change
        }
        let backup = path.with_extension(format!(
            "{}.bak",
            path.extension().and_then(|e| e.to_str()).unwrap_or("bak")
        ));
        if !backup.exists() {
            let _ = fs::copy(path, &backup);
        }
    }
    fs::write(path, content)
}

#[cfg(target_os = "macos")]
fn get_uid() -> Result<String, String> {
    let output = std::process::Command::new("id")
        .arg("-u")
        .output()
        .map_err(|e| format!("failed to get uid: {e}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
