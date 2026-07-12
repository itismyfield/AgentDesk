use std::collections::HashSet;

#[cfg(test)]
use std::{fs, path::PathBuf};

#[cfg(all(test, unix))]
use super::discord_thread_create_lock::ensure_secure_lock_directory;

use poise::serenity_prelude as serenity;
use serde::Deserialize;
use serde_json::{Value, json};
use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParentMode {
    Text,
    News,
    Forum,
    Media,
}

impl ParentMode {
    fn from_channel_type(kind: serenity::ChannelType) -> Result<Self, String> {
        match kind {
            serenity::ChannelType::Text => Ok(Self::Text),
            serenity::ChannelType::News => Ok(Self::News),
            serenity::ChannelType::Forum => Ok(Self::Forum),
            // Discord media channels use type 16. Serenity 0.12 represents
            // that newer type through the non-exhaustive Unknown variant.
            serenity::ChannelType::Unknown(16) => Ok(Self::Media),
            other => Err(format!(
                "unsupported parent channel type {} ({}); expected text, news, forum, or media",
                u8::from(other),
                other.name()
            )),
        }
    }

    fn requires_starter(self) -> bool {
        matches!(self, Self::Forum | Self::Media)
    }

    fn supports_tags(self) -> bool {
        matches!(self, Self::Forum | Self::Media)
    }
}

#[derive(Debug)]
struct ParentInfo {
    guild_id: serenity::GuildId,
    mode: ParentMode,
    flags: serenity::ChannelFlags,
    available_tag_ids: HashSet<u64>,
}

fn normalized_name(name: &str) -> String {
    name.trim().nfkc().collect()
}

#[cfg(all(test, unix))]
fn lock_path(parent_id: serenity::ChannelId, normalized: &str) -> Result<PathBuf, String> {
    let key = format!("{}\0{normalized}", parent_id.get());
    super::discord_thread_create_lock::lock_path(&blake3::hash(key.as_bytes()).to_hex().to_string())
}

fn acquire_thread_create_lock(
    parent_id: serenity::ChannelId,
    normalized: &str,
) -> Result<super::discord_thread_create_lock::ThreadCreateFileLock, String> {
    let key = format!("{}\0{normalized}", parent_id.get());
    super::discord_thread_create_lock::acquire(&blake3::hash(key.as_bytes()).to_hex().to_string())
}

async fn acquire_thread_create_lock_async(
    parent_id: serenity::ChannelId,
    normalized: String,
) -> Result<super::discord_thread_create_lock::ThreadCreateFileLock, String> {
    tokio::task::spawn_blocking(move || acquire_thread_create_lock(parent_id, &normalized))
        .await
        .map_err(|err| format!("join thread-create lock acquisition: {err}"))?
}

fn parse_archive_duration(minutes: u16) -> Result<serenity::AutoArchiveDuration, String> {
    match minutes {
        60 => Ok(serenity::AutoArchiveDuration::OneHour),
        1440 => Ok(serenity::AutoArchiveDuration::OneDay),
        4320 => Ok(serenity::AutoArchiveDuration::ThreeDays),
        10080 => Ok(serenity::AutoArchiveDuration::OneWeek),
        other => Err(format!(
            "auto_archive_minutes must be 60, 1440, 4320, or 10080; got {other}"
        )),
    }
}

fn result_json(
    id: serenity::ChannelId,
    name: &str,
    parent_id: serenity::ChannelId,
    created: bool,
) -> Value {
    json!({
        "id": id.get().to_string(),
        "name": name,
        "kind": "thread",
        "parent_channel_id": parent_id.get().to_string(),
        "created": created,
    })
}

fn json_snowflake(value: &Value, field: &str) -> Result<u64, String> {
    let raw = value
        .get(field)
        .ok_or_else(|| format!("Discord parent channel response omitted {field}"))?;
    raw.as_str()
        .map(str::parse::<u64>)
        .transpose()
        .map_err(|err| format!("invalid {field} in Discord parent channel response: {err}"))?
        .or_else(|| raw.as_u64())
        .ok_or_else(|| format!("invalid {field} in Discord parent channel response"))
}

fn parent_flags(value: &Value) -> Result<serenity::ChannelFlags, String> {
    match value.get("flags") {
        None | Some(Value::Null) => Ok(serenity::ChannelFlags::empty()),
        Some(raw) => raw
            .as_u64()
            .map(serenity::ChannelFlags::from_bits_retain)
            .ok_or_else(|| "invalid flags in Discord parent channel response".to_string()),
    }
}

fn parent_available_tag_ids(value: &Value) -> Result<HashSet<u64>, String> {
    let Some(raw_tags) = value.get("available_tags") else {
        return Ok(HashSet::new());
    };
    let tags = raw_tags.as_array().ok_or_else(|| {
        "invalid available_tags in Discord parent channel response; creation was not attempted"
            .to_string()
    })?;
    tags.iter().map(|tag| json_snowflake(tag, "id")).collect()
}

async fn resolve_parent(
    http: &serenity::Http,
    parent_id: serenity::ChannelId,
) -> Result<ParentInfo, String> {
    // Use the raw endpoint because Serenity 0.12's Channel deserializer
    // rejects Discord's newer media channel type (16).
    let request = serenity::http::Request::new(
        serenity::http::Route::Channel {
            channel_id: parent_id,
        },
        serenity::http::LightMethod::Get,
    );
    let response = http
        .request(request)
        .await
        .map_err(|err| format!("resolve parent channel {parent_id}: {err}"))?;
    let value = response
        .json::<Value>()
        .await
        .map_err(|err| format!("decode parent channel {parent_id}: {err}"))?;
    let guild_id = serenity::GuildId::new(json_snowflake(&value, "guild_id")?);
    let raw_kind = value
        .get("type")
        .and_then(Value::as_u64)
        .and_then(|kind| u8::try_from(kind).ok())
        .ok_or_else(|| "invalid type in Discord parent channel response".to_string())?;
    Ok(ParentInfo {
        guild_id,
        mode: ParentMode::from_channel_type(serenity::ChannelType::from(raw_kind))?,
        flags: parent_flags(&value)?,
        available_tag_ids: parent_available_tag_ids(&value)?,
    })
}

fn validated_tags(info: &ParentInfo, requested: &[u64]) -> Result<Vec<u64>, String> {
    if !info.mode.supports_tags() && !requested.is_empty() {
        return Err(
            "--tag-id is only valid for forum or media parent channels; creation was not attempted"
                .into(),
        );
    }

    let mut seen = HashSet::new();
    let mut applied = Vec::new();
    for tag_id in requested {
        if !info.available_tag_ids.contains(tag_id) {
            return Err(format!(
                "tag id {tag_id} is not available on the parent channel; creation was not attempted"
            ));
        }
        if seen.insert(*tag_id) {
            applied.push(*tag_id);
        }
    }
    if info.flags.contains(serenity::ChannelFlags::REQUIRE_TAG) && applied.is_empty() {
        return Err(
            "parent channel requires at least one available --tag-id; creation was not attempted"
                .into(),
        );
    }
    Ok(applied)
}

fn same_thread_key(
    thread: &serenity::GuildChannel,
    parent_id: serenity::ChannelId,
    name: &str,
) -> bool {
    thread.parent_id == Some(parent_id) && normalized_name(&thread.name) == name
}

async fn archived_public_page(
    http: &serenity::Http,
    parent_id: serenity::ChannelId,
    before: Option<&str>,
) -> Result<ArchivedPublicPage, String> {
    let mut params = vec![("limit", "100".to_string())];
    if let Some(before) = before {
        params.push(("before", before.to_string()));
    }
    let request = serenity::http::Request::new(
        serenity::http::Route::ChannelArchivedPublicThreads {
            channel_id: parent_id,
        },
        serenity::http::LightMethod::Get,
    )
    .params(Some(params));
    let response = http.request(request).await.map_err(|err| {
        format!("list archived public threads under {parent_id}: {err}; creation was not attempted")
    })?;
    response.json::<ArchivedPublicPage>().await.map_err(|err| {
        format!(
            "decode archived public threads under {parent_id}: {err}; creation was not attempted"
        )
    })
}

/// Archived-list responses require `has_more`. Serenity's shared
/// `ThreadsData` model defaults that field because the active-thread endpoint
/// omits it; using that model here would turn a malformed/partial archived
/// response into a conclusive miss and permit a duplicate create.
#[derive(Debug, Deserialize)]
struct ArchivedPublicPage {
    threads: Vec<serenity::GuildChannel>,
    has_more: bool,
}

async fn find_existing_thread(
    http: &serenity::Http,
    guild_id: serenity::GuildId,
    parent_id: serenity::ChannelId,
    name: &str,
) -> Result<Option<serenity::GuildChannel>, String> {
    let active = guild_id.get_active_threads(http).await.map_err(|err| {
        format!("list active threads in guild {guild_id}: {err}; creation was not attempted")
    })?;
    if let Some(thread) = active
        .threads
        .into_iter()
        .find(|thread| same_thread_key(thread, parent_id, name))
    {
        return Ok(Some(thread));
    }

    let mut before = None::<String>;
    let mut previous_cursor = None::<serenity::Timestamp>;
    let mut seen_cursors = HashSet::new();
    loop {
        let page = archived_public_page(http, parent_id, before.as_deref()).await?;
        if let Some(thread) = page
            .threads
            .iter()
            .find(|thread| same_thread_key(thread, parent_id, name))
        {
            return Ok(Some(thread.clone()));
        }
        if !page.has_more {
            return Ok(None);
        }
        let next = page
            .threads
            .last()
            .and_then(|thread| thread.thread_metadata)
            .and_then(|metadata| metadata.archive_timestamp)
            .ok_or_else(|| {
                format!(
                    "archived thread page under {parent_id} has_more but no archive cursor; creation was not attempted"
                )
            })?;
        let next_string = next.to_string();
        if previous_cursor.is_some_and(|previous| next >= previous) {
            return Err(format!(
                "archived thread pagination under {parent_id} did not strictly progress before {next_string}; creation was not attempted"
            ));
        }
        if !seen_cursors.insert(next_string.clone()) {
            return Err(format!(
                "archived thread pagination under {parent_id} repeated cursor {next_string}; creation was not attempted"
            ));
        }
        previous_cursor = Some(next);
        before = Some(next_string);
    }
}

async fn create_forum_post(
    http: &serenity::Http,
    parent_id: serenity::ChannelId,
    name: &str,
    starter: &str,
    applied_tags: &[u64],
    auto_archive_minutes: u16,
) -> Result<serenity::GuildChannel, String> {
    let body = serde_json::to_vec(&json!({
        "name": name,
        "auto_archive_duration": auto_archive_minutes,
        "message": { "content": starter },
        "applied_tags": applied_tags.iter().map(u64::to_string).collect::<Vec<_>>(),
    }))
    .map_err(|err| format!("serialize forum post {name:?}: {err}"))?;
    let request = serenity::http::Request::new(
        serenity::http::Route::ChannelForumPosts {
            channel_id: parent_id,
        },
        serenity::http::LightMethod::Post,
    )
    .body(Some(body));
    let response = http
        .request(request)
        .await
        .map_err(|err| format!("create forum/media post {name:?} under {parent_id}: {err}"))?;
    response
        .json::<serenity::GuildChannel>()
        .await
        .map_err(|err| format!("decode created forum/media post {name:?}: {err}"))
}

pub(super) async fn create(
    http: &serenity::Http,
    parent_channel_id: &str,
    name: &str,
    message: Option<&str>,
    tag_ids: &[u64],
    auto_archive_minutes: u16,
) -> Result<Value, String> {
    let parent_id = parent_channel_id
        .parse::<u64>()
        .map(serenity::ChannelId::new)
        .map_err(|err| format!("invalid parent channel id {parent_channel_id:?}: {err}"))?;
    let archive = parse_archive_duration(auto_archive_minutes)?;
    let request_name = name.trim();
    if request_name.is_empty() {
        return Err("thread name must not be empty".into());
    }
    let normalized = normalized_name(request_name);
    let starter = message.filter(|message| !message.trim().is_empty());

    // The OS file lock spans parent resolution, every lookup page, and the
    // create request. Retries from separate AgentDesk processes therefore
    // cannot both observe a miss and create the same logical thread.
    let _lock = acquire_thread_create_lock_async(parent_id, normalized.clone()).await?;
    let parent = resolve_parent(http, parent_id).await?;
    if let Some(thread) =
        find_existing_thread(http, parent.guild_id, parent_id, &normalized).await?
    {
        return Ok(result_json(thread.id, &thread.name, parent_id, false));
    }

    if parent.mode.requires_starter() && starter.is_none() {
        return Err(format!(
            "{} parent channel {parent_id} requires a non-empty starter message; pass --message <TEXT> (or --starter-message <TEXT>)",
            match parent.mode {
                ParentMode::Forum => "forum",
                ParentMode::Media => "media",
                ParentMode::Text | ParentMode::News => {
                    unreachable!("text/news parents do not require starter messages")
                }
            }
        ));
    }
    let applied_tags = validated_tags(&parent, tag_ids)?;

    let thread = match parent.mode {
        ParentMode::Text | ParentMode::News => {
            let kind = match parent.mode {
                ParentMode::Text => serenity::ChannelType::PublicThread,
                ParentMode::News => serenity::ChannelType::NewsThread,
                ParentMode::Forum | ParentMode::Media => unreachable!(),
            };
            let builder = serenity::CreateThread::new(request_name)
                .kind(kind)
                .auto_archive_duration(archive);
            parent_id
                .create_thread(http, builder)
                .await
                .map_err(|err| format!("create thread {request_name:?} under {parent_id}: {err}"))?
        }
        ParentMode::Forum | ParentMode::Media => {
            create_forum_post(
                http,
                parent_id,
                request_name,
                starter.expect("forum/media starter presence was validated before lookup"),
                &applied_tags,
                auto_archive_minutes,
            )
            .await?
        }
    };

    Ok(result_json(thread.id, &thread.name, parent_id, true))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::{
        Json, Router,
        extract::{Path, Query, State},
        http::StatusCode,
        response::{IntoResponse, Response},
        routing::{get, post},
    };
    use serde::Deserialize;
    use tokio::sync::Notify;

    use super::*;

    const PARENT_ID: u64 = 100;
    const GUILD_ID: u64 = 200;
    const THREAD_ID: u64 = 300;

    #[derive(Clone)]
    struct ArchivedMockPage {
        before: Option<String>,
        threads: Vec<Value>,
        has_more: Option<Value>,
        status: StatusCode,
    }

    impl ArchivedMockPage {
        fn ok(before: Option<&str>, threads: Vec<Value>, has_more: bool) -> Self {
            Self {
                before: before.map(str::to_string),
                threads,
                has_more: Some(json!(has_more)),
                status: StatusCode::OK,
            }
        }

        fn malformed_has_more(has_more: Option<Value>) -> Self {
            Self {
                before: None,
                threads: Vec::new(),
                has_more,
                status: StatusCode::OK,
            }
        }
    }

    #[derive(Clone)]
    struct MockState {
        parent_kind: serenity::ChannelType,
        parent_flags: u64,
        available_tag_ids: Vec<u64>,
        active_status: StatusCode,
        active_threads: Vec<Value>,
        active_gets: Arc<AtomicUsize>,
        archived_pages: Vec<ArchivedMockPage>,
        archived_gets: Arc<AtomicUsize>,
        reflect_posts_in_active: bool,
        first_active_started: Option<Arc<Notify>>,
        release_first_active: Option<Arc<Notify>>,
        second_active_started: Option<Arc<Notify>>,
        post_bodies: Arc<Mutex<Vec<Value>>>,
    }

    fn channel_json(
        id: u64,
        guild_id: u64,
        name: &str,
        kind: serenity::ChannelType,
        parent_id: Option<u64>,
    ) -> Value {
        let mut channel = serenity::GuildChannel::default();
        channel.id = serenity::ChannelId::new(id);
        channel.guild_id = serenity::GuildId::new(guild_id);
        channel.name = name.to_string();
        channel.kind = kind;
        channel.parent_id = parent_id.map(serenity::ChannelId::new);
        serde_json::to_value(channel).expect("serialize mock guild channel")
    }

    fn archived_channel_json(id: u64, name: &str, timestamp: &str) -> Value {
        let mut value = channel_json(
            id,
            GUILD_ID,
            name,
            serenity::ChannelType::PublicThread,
            Some(PARENT_ID),
        );
        value["thread_metadata"] = json!({
            "archived": true,
            "auto_archive_duration": 1440,
            "archive_timestamp": timestamp,
            "locked": false,
            "create_timestamp": null,
        });
        value
    }

    async fn get_parent(State(state): State<MockState>, Path(_id): Path<u64>) -> Json<Value> {
        Json(json!({
            "id": PARENT_ID.to_string(),
            "guild_id": GUILD_ID.to_string(),
            "name": "parent",
            "type": u8::from(state.parent_kind),
            "flags": state.parent_flags,
            "available_tags": state.available_tag_ids.iter().map(|id| json!({
                "id": id.to_string(),
                "name": format!("tag-{id}"),
                "moderated": false,
                "emoji_id": null,
                "emoji_name": null,
            })).collect::<Vec<_>>(),
        }))
    }

    async fn get_active(State(state): State<MockState>, Path(_id): Path<u64>) -> Response {
        let request_number = state.active_gets.fetch_add(1, Ordering::SeqCst) + 1;
        if request_number == 1 {
            if let Some(started) = &state.first_active_started {
                started.notify_one();
            }
            if let Some(release) = &state.release_first_active {
                release.notified().await;
            }
        } else if request_number == 2
            && let Some(started) = &state.second_active_started
        {
            started.notify_one();
        }
        if !state.active_status.is_success() {
            return (
                state.active_status,
                Json(json!({"message": "active thread lookup failed", "code": 0})),
            )
                .into_response();
        }
        let mut threads = state.active_threads.clone();
        if state.reflect_posts_in_active
            && let Some(body) = state.post_bodies.lock().unwrap().last()
        {
            threads.push(channel_json(
                THREAD_ID,
                GUILD_ID,
                body["name"].as_str().unwrap_or("created"),
                serenity::ChannelType::PublicThread,
                Some(PARENT_ID),
            ));
        }
        Json(json!({"threads": threads, "members": []})).into_response()
    }

    #[derive(Deserialize)]
    struct ArchivedQuery {
        before: Option<String>,
        limit: Option<u64>,
    }

    async fn get_archived(
        State(state): State<MockState>,
        Path(_id): Path<u64>,
        Query(query): Query<ArchivedQuery>,
    ) -> Response {
        state.archived_gets.fetch_add(1, Ordering::SeqCst);
        assert_eq!(query.limit, Some(100));
        let Some(page) = state
            .archived_pages
            .iter()
            .find(|page| page.before == query.before)
        else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "message": format!("unexpected before cursor {:?}", query.before),
                    "code": 0
                })),
            )
                .into_response();
        };
        if !page.status.is_success() {
            return (
                page.status,
                Json(json!({"message": "archived lookup failed", "code": 0})),
            )
                .into_response();
        }
        let mut response = json!({
            "threads": page.threads.clone(),
            "members": [],
        });
        if let Some(has_more) = &page.has_more {
            response["has_more"] = has_more.clone();
        }
        Json(response).into_response()
    }

    async fn create_thread(
        State(state): State<MockState>,
        Path(_id): Path<u64>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        state.post_bodies.lock().unwrap().push(body.clone());
        let kind = match body["type"].as_u64() {
            Some(10) => serenity::ChannelType::NewsThread,
            _ => serenity::ChannelType::PublicThread,
        };
        Json(channel_json(
            THREAD_ID,
            GUILD_ID,
            body["name"].as_str().unwrap_or("created"),
            kind,
            Some(PARENT_ID),
        ))
    }

    async fn mock_http(state: MockState) -> (serenity::Http, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/api/v10/channels/{id}", get(get_parent))
            .route("/api/v10/guilds/{id}/threads/active", get(get_active))
            .route(
                "/api/v10/channels/{id}/threads/archived/public",
                get(get_archived),
            )
            .route("/api/v10/channels/{id}/threads", post(create_thread))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let http = serenity::HttpBuilder::new("test-token")
            .proxy(proxy)
            .ratelimiter_disabled(true)
            .build();
        (http, server)
    }

    fn mock_state(parent_kind: serenity::ChannelType) -> MockState {
        MockState {
            parent_kind,
            parent_flags: 0,
            available_tag_ids: Vec::new(),
            active_status: StatusCode::OK,
            active_threads: Vec::new(),
            active_gets: Arc::new(AtomicUsize::new(0)),
            archived_pages: vec![ArchivedMockPage::ok(None, Vec::new(), false)],
            archived_gets: Arc::new(AtomicUsize::new(0)),
            reflect_posts_in_active: false,
            first_active_started: None,
            release_first_active: None,
            second_active_started: None,
            post_bodies: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[tokio::test]
    async fn forum_parent_posts_starter_message_tags_and_returns_json() {
        let mut state = mock_state(serenity::ChannelType::Forum);
        state.available_tag_ids = vec![41, 42];
        let bodies = state.post_bodies.clone();
        let (http, server) = mock_http(state).await;

        let result = create(
            &http,
            "100",
            "release-notes",
            Some("  **First post**  \n"),
            &[42, 41, 42],
            1440,
        )
        .await
        .expect("forum post succeeds");
        server.abort();

        assert_eq!(result["created"], true);
        assert_eq!(result["id"], "300");
        assert_eq!(result["name"], "release-notes");
        assert_eq!(result["parent_channel_id"], "100");
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["message"]["content"], "  **First post**  \n");
        assert_eq!(bodies[0]["applied_tags"], json!(["42", "41"]));
        assert_eq!(bodies[0]["auto_archive_duration"], 1440);
        assert!(bodies[0].get("type").is_none());
    }

    #[tokio::test]
    async fn media_parent_uses_forum_post_payload() {
        let state = mock_state(serenity::ChannelType::Unknown(16));
        let bodies = state.post_bodies.clone();
        let (http, server) = mock_http(state).await;

        create(&http, "100", "media-post", Some("media starter"), &[], 60)
            .await
            .expect("media post succeeds");
        server.abort();

        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies[0]["message"]["content"], "media starter");
        assert_eq!(bodies[0]["applied_tags"], json!([]));
    }

    #[tokio::test]
    async fn required_or_unavailable_tags_fail_after_conclusive_lookup_without_post() {
        let mut state = mock_state(serenity::ChannelType::Forum);
        state.parent_flags = serenity::ChannelFlags::REQUIRE_TAG.bits();
        state.available_tag_ids = vec![41];
        let active_gets = state.active_gets.clone();
        let archived_gets = state.archived_gets.clone();
        let bodies = state.post_bodies.clone();
        let (http, server) = mock_http(state).await;

        let missing = create(&http, "100", "missing-tag", Some("starter"), &[], 1440)
            .await
            .expect_err("required tag must fail closed");
        let unavailable = create(
            &http,
            "100",
            "unavailable-tag",
            Some("starter"),
            &[99],
            1440,
        )
        .await
        .expect_err("unavailable tag must fail closed");
        server.abort();

        assert!(missing.contains("requires at least one"));
        assert!(unavailable.contains("not available"));
        assert_eq!(active_gets.load(Ordering::SeqCst), 2);
        assert_eq!(archived_gets.load(Ordering::SeqCst), 2);
        assert!(bodies.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn forum_parent_without_starter_fails_after_lookup_without_post() {
        let state = mock_state(serenity::ChannelType::Forum);
        let active_gets = state.active_gets.clone();
        let archived_gets = state.archived_gets.clone();
        let bodies = state.post_bodies.clone();
        let (http, server) = mock_http(state).await;

        let error = create(&http, "100", "missing-starter", Some("   "), &[], 1440)
            .await
            .expect_err("empty forum starter must fail");
        server.abort();

        assert!(error.contains("requires a non-empty starter message"));
        assert_eq!(active_gets.load(Ordering::SeqCst), 1);
        assert_eq!(archived_gets.load(Ordering::SeqCst), 1);
        assert!(bodies.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn text_and_news_parents_use_distinct_thread_types() {
        for (parent_kind, expected_type) in [
            (serenity::ChannelType::Text, 11),
            (serenity::ChannelType::News, 10),
        ] {
            let state = mock_state(parent_kind);
            let bodies = state.post_bodies.clone();
            let (http, server) = mock_http(state).await;

            create(
                &http,
                "100",
                &format!("kind-{expected_type}"),
                None,
                &[],
                4320,
            )
            .await
            .expect("thread succeeds");
            server.abort();

            let bodies = bodies.lock().unwrap();
            assert_eq!(bodies.len(), 1);
            assert_eq!(bodies[0]["type"], expected_type);
            assert_eq!(bodies[0]["auto_archive_duration"], 4320);
            assert!(bodies[0].get("message").is_none());
        }
    }

    #[tokio::test]
    async fn existing_active_text_and_forum_threads_are_idempotent() {
        for parent_kind in [serenity::ChannelType::Text, serenity::ChannelType::Forum] {
            let mut state = mock_state(parent_kind);
            state.parent_flags = serenity::ChannelFlags::REQUIRE_TAG.bits();
            state.active_threads.push(channel_json(
                THREAD_ID,
                GUILD_ID,
                "already-there",
                serenity::ChannelType::PublicThread,
                Some(PARENT_ID),
            ));
            let bodies = state.post_bodies.clone();
            let archived_gets = state.archived_gets.clone();
            let (http, server) = mock_http(state).await;

            let result = create(&http, "100", "already-there", None, &[99], 1440)
                .await
                .expect("active hit succeeds");
            server.abort();

            assert_eq!(
                result,
                json!({
                    "id": "300",
                    "name": "already-there",
                    "kind": "thread",
                    "parent_channel_id": "100",
                    "created": false,
                })
            );
            assert_eq!(archived_gets.load(Ordering::SeqCst), 0);
            assert!(bodies.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn active_miss_archived_hit_is_idempotent() {
        let mut state = mock_state(serenity::ChannelType::Forum);
        state.parent_flags = serenity::ChannelFlags::REQUIRE_TAG.bits();
        state.archived_pages = vec![ArchivedMockPage::ok(
            None,
            vec![archived_channel_json(
                THREAD_ID,
                "already-archived",
                "2026-07-12T00:00:00.000Z",
            )],
            false,
        )];
        let bodies = state.post_bodies.clone();
        let (http, server) = mock_http(state).await;

        let result = create(&http, "100", "already-archived", None, &[99], 1440)
            .await
            .expect("archived hit succeeds");
        server.abort();

        assert_eq!(result["created"], false);
        assert!(bodies.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn archived_later_page_hit_is_idempotent() {
        let mut state = mock_state(serenity::ChannelType::Text);
        let first_cursor = "2026-07-12T01:00:00.000Z";
        state.archived_pages = vec![
            ArchivedMockPage::ok(
                None,
                vec![archived_channel_json(301, "other", first_cursor)],
                true,
            ),
            ArchivedMockPage::ok(
                Some(first_cursor),
                vec![archived_channel_json(
                    THREAD_ID,
                    "later-hit",
                    "2026-07-11T01:00:00.000Z",
                )],
                false,
            ),
        ];
        let archived_gets = state.archived_gets.clone();
        let bodies = state.post_bodies.clone();
        let (http, server) = mock_http(state).await;

        let result = create(&http, "100", "later-hit", None, &[], 1440)
            .await
            .expect("later archived page hit succeeds");
        server.abort();

        assert_eq!(result["created"], false);
        assert_eq!(archived_gets.load(Ordering::SeqCst), 2);
        assert!(bodies.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn later_archived_page_failure_never_posts() {
        let mut state = mock_state(serenity::ChannelType::Text);
        let first_cursor = "2026-07-12T01:00:00.000Z";
        state.archived_pages = vec![
            ArchivedMockPage::ok(
                None,
                vec![archived_channel_json(301, "other", first_cursor)],
                true,
            ),
            ArchivedMockPage {
                before: Some(first_cursor.to_string()),
                threads: Vec::new(),
                has_more: Some(json!(false)),
                status: StatusCode::INTERNAL_SERVER_ERROR,
            },
        ];
        let bodies = state.post_bodies.clone();
        let (http, server) = mock_http(state).await;

        let error = create(&http, "100", "must-not-create", None, &[], 1440)
            .await
            .expect_err("later lookup failure aborts creation");
        server.abort();

        assert!(error.contains("creation was not attempted"));
        assert!(bodies.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_archived_has_more_never_posts() {
        for (label, has_more) in [
            ("missing", None),
            ("null", Some(Value::Null)),
            ("wrong-type", Some(json!("false"))),
        ] {
            let mut state = mock_state(serenity::ChannelType::Text);
            state.archived_pages = vec![ArchivedMockPage::malformed_has_more(has_more)];
            let bodies = state.post_bodies.clone();
            let (http, server) = mock_http(state).await;

            let error = create(
                &http,
                "100",
                &format!("must-not-create-{label}"),
                None,
                &[],
                1440,
            )
            .await
            .expect_err("malformed archived pagination must fail closed");
            server.abort();

            assert!(
                error.contains("decode archived public threads"),
                "unexpected {label} error: {error}"
            );
            assert!(
                bodies.lock().unwrap().is_empty(),
                "{label} has_more response reached POST"
            );
        }
    }

    #[tokio::test]
    async fn archived_three_page_cursor_cycle_fails_closed() {
        let mut state = mock_state(serenity::ChannelType::Text);
        let cursor_a = "2026-07-12T03:00:00.000Z";
        let cursor_b = "2026-07-12T02:00:00.000Z";
        state.archived_pages = vec![
            ArchivedMockPage::ok(
                None,
                vec![archived_channel_json(301, "page-a", cursor_a)],
                true,
            ),
            ArchivedMockPage::ok(
                Some(cursor_a),
                vec![archived_channel_json(302, "page-b", cursor_b)],
                true,
            ),
            ArchivedMockPage::ok(
                Some(cursor_b),
                vec![archived_channel_json(303, "cycle-a", cursor_a)],
                true,
            ),
        ];
        let gets = state.archived_gets.clone();
        let bodies = state.post_bodies.clone();
        let (http, server) = mock_http(state).await;

        let error = create(&http, "100", "absent", None, &[], 1440)
            .await
            .expect_err("A -> B -> A cursor cycle must fail closed");
        server.abort();

        assert!(error.contains("strictly progress") || error.contains("repeated cursor"));
        assert_eq!(gets.load(Ordering::SeqCst), 3);
        assert!(bodies.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn archived_repeated_cursor_and_empty_has_more_fail_closed() {
        let cursor = "2026-07-12T03:00:00.000Z";
        let cases = [
            vec![
                ArchivedMockPage::ok(
                    None,
                    vec![archived_channel_json(301, "first", cursor)],
                    true,
                ),
                ArchivedMockPage::ok(
                    Some(cursor),
                    vec![archived_channel_json(302, "repeat", cursor)],
                    true,
                ),
            ],
            vec![ArchivedMockPage::ok(None, Vec::new(), true)],
        ];

        for (index, pages) in cases.into_iter().enumerate() {
            let mut state = mock_state(serenity::ChannelType::Text);
            state.archived_pages = pages;
            let bodies = state.post_bodies.clone();
            let (http, server) = mock_http(state).await;

            let error = create(&http, "100", &format!("absent-{index}"), None, &[], 1440)
                .await
                .expect_err("non-progressing pagination must fail closed");
            server.abort();

            assert!(error.contains("creation was not attempted"));
            assert!(bodies.lock().unwrap().is_empty());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn os_file_lock_serializes_lookup_and_create_interleaving() {
        let mut state = mock_state(serenity::ChannelType::Text);
        state.reflect_posts_in_active = true;
        let first_started = Arc::new(Notify::new());
        let release_first = Arc::new(Notify::new());
        let second_started = Arc::new(Notify::new());
        state.first_active_started = Some(first_started.clone());
        state.release_first_active = Some(release_first.clone());
        state.second_active_started = Some(second_started.clone());
        let bodies = state.post_bodies.clone();
        let (http, server) = mock_http(state).await;
        let http = Arc::new(http);

        let first_http = http.clone();
        let first =
            tokio::spawn(
                async move { create(&first_http, "100", "same-name", None, &[], 1440).await },
            );
        first_started.notified().await;
        let second_http = http.clone();
        let second =
            tokio::spawn(
                async move { create(&second_http, "100", "same-name", None, &[], 1440).await },
            );

        // Mutation tooth: without the file flock the second invocation reaches
        // the active-thread snapshot while the first is deliberately paused.
        let crossed_lock =
            tokio::time::timeout(Duration::from_millis(150), second_started.notified())
                .await
                .is_ok();
        release_first.notify_one();
        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();
        server.abort();

        assert!(!crossed_lock, "second lookup crossed the kernel flock");
        assert_eq!(first["created"], true);
        assert_eq!(second["created"], false);
        assert_eq!(bodies.lock().unwrap().len(), 1);
    }

    #[test]
    #[ignore = "helper subprocess for the cross-process file-lock test"]
    fn thread_create_lock_child_process() {
        let Some(name) = std::env::var_os("ADK_THREAD_LOCK_CHILD_NAME") else {
            return;
        };
        let name = name.to_string_lossy();
        // Keep the runtime alive until after the guard drops. This mirrors the
        // one-shot CLI and catches a Windows mutex acquired on a Tokio
        // blocking worker but incorrectly released from the caller thread.
        let runtime = std::env::var_os("ADK_THREAD_LOCK_USE_ASYNC")
            .map(|_| tokio::runtime::Runtime::new().expect("child Tokio runtime"));
        let lock = match runtime.as_ref() {
            Some(runtime) => runtime
                .block_on(acquire_thread_create_lock_async(
                    serenity::ChannelId::new(PARENT_ID),
                    name.to_string(),
                ))
                .expect("child asynchronously acquires OS file lock"),
            None => acquire_thread_create_lock(serenity::ChannelId::new(PARENT_ID), &name)
                .expect("child acquires OS file lock"),
        };
        let acquired = PathBuf::from(
            std::env::var_os("ADK_THREAD_LOCK_ACQUIRED").expect("acquired marker path"),
        );
        fs::write(&acquired, b"acquired").unwrap();

        if let Some(ready) = std::env::var_os("ADK_THREAD_LOCK_READY") {
            fs::write(ready, b"ready").unwrap();
            let release = PathBuf::from(
                std::env::var_os("ADK_THREAD_LOCK_RELEASE").expect("release marker path"),
            );
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !release.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(release.exists(), "parent did not release helper lock");
        }
        drop(lock);
        if let Some(released) = std::env::var_os("ADK_THREAD_LOCK_RELEASED") {
            fs::write(released, b"released").unwrap();
        }
        if let Some(exit) = std::env::var_os("ADK_THREAD_LOCK_EXIT") {
            let exit = PathBuf::from(exit);
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !exit.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(exit.exists(), "parent did not let helper runtime exit");
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "helper subprocess for Windows async lock cancellation"]
    fn thread_create_lock_cancel_child_process() {
        let name = std::env::var("ADK_THREAD_LOCK_CHILD_NAME").expect("child lock name");
        let runtime = tokio::runtime::Runtime::new().expect("child Tokio runtime");
        let task = runtime.block_on(async {
            let lock = acquire_thread_create_lock_async(
                serenity::ChannelId::new(PARENT_ID),
                name.to_string(),
            )
            .await
            .expect("child asynchronously acquires OS file lock");
            tokio::spawn(async move {
                let _lock = lock;
                std::future::pending::<()>().await;
            })
        });
        fs::write(
            std::env::var_os("ADK_THREAD_LOCK_ACQUIRED").expect("acquired marker path"),
            b"acquired",
        )
        .unwrap();
        fs::write(
            std::env::var_os("ADK_THREAD_LOCK_READY").expect("ready marker path"),
            b"ready",
        )
        .unwrap();

        let cancel =
            PathBuf::from(std::env::var_os("ADK_THREAD_LOCK_CANCEL").expect("cancel marker path"));
        wait_for_path(&cancel, Duration::from_secs(10));
        task.abort();
        let cancelled = runtime
            .block_on(task)
            .expect_err("lock holder task must be cancelled");
        assert!(cancelled.is_cancelled());
        fs::write(
            std::env::var_os("ADK_THREAD_LOCK_CANCELLED").expect("cancelled marker path"),
            b"cancelled",
        )
        .unwrap();

        // Keep the runtime (and, in the old broken implementation, its mutex-
        // owning blocking worker) alive until the parent has observed whether
        // cancellation really released the named mutex.
        let exit = PathBuf::from(
            std::env::var_os("ADK_THREAD_LOCK_EXIT").expect("runtime exit marker path"),
        );
        wait_for_path(&exit, Duration::from_secs(10));
    }

    fn wait_for_path(path: &std::path::Path, timeout: Duration) {
        assert!(
            path_appears(path, timeout),
            "timed out waiting for {}",
            path.display()
        );
    }

    fn path_appears(path: &std::path::Path, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while !path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        path.exists()
    }

    fn wait_for_child(mut child: std::process::Child) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "lock helper failed: {status}");
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for lock helper"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn async_os_file_lock_serializes_actual_child_processes() {
        let temp = tempfile::tempdir().unwrap();
        let ready = temp.path().join("first-ready");
        let release = temp.path().join("release-first");
        let released = temp.path().join("first-released");
        let exit = temp.path().join("exit-first-runtime");
        let first_acquired = temp.path().join("first-acquired");
        let second_acquired = temp.path().join("second-acquired");
        let unique_name = format!(
            "child-process-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let executable = std::env::current_exe().unwrap();
        let helper = "cli::discord_thread_create::tests::thread_create_lock_child_process";
        let first = std::process::Command::new(&executable)
            .args(["--ignored", "--exact", helper])
            .env("ADK_THREAD_LOCK_USE_ASYNC", "1")
            .env("ADK_THREAD_LOCK_CHILD_NAME", &unique_name)
            .env("ADK_THREAD_LOCK_ACQUIRED", &first_acquired)
            .env("ADK_THREAD_LOCK_READY", &ready)
            .env("ADK_THREAD_LOCK_RELEASE", &release)
            .env("ADK_THREAD_LOCK_RELEASED", &released)
            .env("ADK_THREAD_LOCK_EXIT", &exit)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        wait_for_path(&ready, Duration::from_secs(10));

        let second = std::process::Command::new(&executable)
            .args(["--ignored", "--exact", helper])
            .env("ADK_THREAD_LOCK_USE_ASYNC", "1")
            .env("ADK_THREAD_LOCK_CHILD_NAME", &unique_name)
            .env("ADK_THREAD_LOCK_ACQUIRED", &second_acquired)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !second_acquired.exists(),
            "second process crossed the first process's OS file lock"
        );

        fs::write(&release, b"release").unwrap();
        wait_for_path(&released, Duration::from_secs(10));
        let acquired_while_first_runtime_alive =
            path_appears(&second_acquired, Duration::from_secs(2));
        fs::write(&exit, b"exit").unwrap();
        wait_for_child(first);
        if !acquired_while_first_runtime_alive {
            wait_for_path(&second_acquired, Duration::from_secs(10));
        }
        wait_for_child(second);
        assert!(
            acquired_while_first_runtime_alive,
            "second process acquired only after the first Tokio runtime exited; the mutex was not released by its dedicated owner thread"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_async_waiter_recovers_abandoned_owner() {
        let temp = tempfile::tempdir().unwrap();
        let ready = temp.path().join("first-ready");
        let never_release = temp.path().join("never-release");
        let first_acquired = temp.path().join("first-acquired");
        let second_acquired = temp.path().join("second-acquired");
        let second_wait_started = temp.path().join("second-wait-started");
        let unique_name = format!(
            "windows-abandoned-child-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let executable = std::env::current_exe().unwrap();
        let helper = "cli::discord_thread_create::tests::thread_create_lock_child_process";
        let mut first = std::process::Command::new(&executable)
            .args(["--ignored", "--exact", helper])
            .env("ADK_THREAD_LOCK_USE_ASYNC", "1")
            .env("ADK_THREAD_LOCK_CHILD_NAME", &unique_name)
            .env("ADK_THREAD_LOCK_ACQUIRED", &first_acquired)
            .env("ADK_THREAD_LOCK_READY", &ready)
            .env("ADK_THREAD_LOCK_RELEASE", &never_release)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        wait_for_path(&ready, Duration::from_secs(10));

        let second = std::process::Command::new(&executable)
            .args(["--ignored", "--exact", helper])
            .env("ADK_THREAD_LOCK_USE_ASYNC", "1")
            .env("ADK_THREAD_LOCK_CHILD_NAME", &unique_name)
            .env("ADK_THREAD_LOCK_ACQUIRED", &second_acquired)
            .env("ADK_THREAD_LOCK_WAIT_STARTED", &second_wait_started)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        wait_for_path(&second_wait_started, Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !second_acquired.exists(),
            "second process must wait before the owner is abandoned"
        );

        first.kill().expect("terminate first lock owner");
        let status = first.wait().expect("reap abandoned lock owner");
        assert!(
            !status.success(),
            "terminated lock owner unexpectedly succeeded"
        );
        wait_for_path(&second_acquired, Duration::from_secs(10));
        wait_for_child(second);
    }

    #[cfg(windows)]
    #[test]
    fn windows_cancelled_async_holder_releases_before_runtime_exit() {
        let temp = tempfile::tempdir().unwrap();
        let ready = temp.path().join("first-ready");
        let cancel = temp.path().join("cancel-first");
        let cancelled = temp.path().join("first-cancelled");
        let exit = temp.path().join("exit-first-runtime");
        let first_acquired = temp.path().join("first-acquired");
        let second_acquired = temp.path().join("second-acquired");
        let second_wait_started = temp.path().join("second-wait-started");
        let unique_name = format!(
            "windows-cancelled-child-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let executable = std::env::current_exe().unwrap();
        let cancel_helper =
            "cli::discord_thread_create::tests::thread_create_lock_cancel_child_process";
        let helper = "cli::discord_thread_create::tests::thread_create_lock_child_process";
        let first = std::process::Command::new(&executable)
            .args(["--ignored", "--exact", cancel_helper])
            .env("ADK_THREAD_LOCK_CHILD_NAME", &unique_name)
            .env("ADK_THREAD_LOCK_ACQUIRED", &first_acquired)
            .env("ADK_THREAD_LOCK_READY", &ready)
            .env("ADK_THREAD_LOCK_CANCEL", &cancel)
            .env("ADK_THREAD_LOCK_CANCELLED", &cancelled)
            .env("ADK_THREAD_LOCK_EXIT", &exit)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        wait_for_path(&ready, Duration::from_secs(10));

        let second = std::process::Command::new(&executable)
            .args(["--ignored", "--exact", helper])
            .env("ADK_THREAD_LOCK_USE_ASYNC", "1")
            .env("ADK_THREAD_LOCK_CHILD_NAME", &unique_name)
            .env("ADK_THREAD_LOCK_ACQUIRED", &second_acquired)
            .env("ADK_THREAD_LOCK_WAIT_STARTED", &second_wait_started)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        wait_for_path(&second_wait_started, Duration::from_secs(10));
        assert!(!second_acquired.exists());

        fs::write(&cancel, b"cancel").unwrap();
        wait_for_path(&cancelled, Duration::from_secs(10));
        let acquired_while_first_runtime_alive =
            path_appears(&second_acquired, Duration::from_secs(2));
        fs::write(&exit, b"exit").unwrap();
        wait_for_child(first);
        if !acquired_while_first_runtime_alive {
            wait_for_path(&second_acquired, Duration::from_secs(10));
        }
        wait_for_child(second);
        assert!(
            acquired_while_first_runtime_alive,
            "task cancellation released the mutex only when the Tokio runtime exited"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_lock_directory_and_file_reject_symlinks_and_weak_permissions() {
        use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, symlink};

        let temp = tempfile::tempdir().unwrap();
        let weak = temp.path().join("weak");
        fs::DirBuilder::new().mode(0o755).create(&weak).unwrap();
        assert!(ensure_secure_lock_directory(&weak).is_err());

        let target = temp.path().join("target");
        fs::DirBuilder::new().mode(0o700).create(&target).unwrap();
        let linked = temp.path().join("linked");
        symlink(&target, &linked).unwrap();
        assert!(ensure_secure_lock_directory(&linked).is_err());

        let unique_name = format!("symlink-file-{}", std::process::id());
        let path = lock_path(serenity::ChannelId::new(PARENT_ID), &unique_name).unwrap();
        let target_file = temp.path().join("target-file");
        fs::write(&target_file, b"target").unwrap();
        let _ = fs::remove_file(&path);
        symlink(&target_file, &path).unwrap();
        assert!(
            acquire_thread_create_lock(serenity::ChannelId::new(PARENT_ID), &unique_name).is_err()
        );
        fs::remove_file(&path).unwrap();

        let guard = acquire_thread_create_lock(
            serenity::ChannelId::new(PARENT_ID),
            &format!("verified-file-{}", std::process::id()),
        )
        .unwrap();
        let metadata = guard.file.metadata().unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.mode() & 0o777, 0o600);
    }

    #[cfg(windows)]
    #[test]
    fn windows_lock_uses_global_current_sid_named_mutex() {
        let normalized = format!("windows-lock-{}", std::process::id());
        let lock_key = format!("{}\0{normalized}", PARENT_ID);
        let lock_hash = blake3::hash(lock_key.as_bytes()).to_hex().to_string();
        let name =
            super::super::discord_thread_create_lock::windows::current_mutex_name(&lock_hash)
                .expect("current-user SID mutex name");
        assert!(name.starts_with("Global\\AgentDesk.ThreadCreate.S-"));
        assert!(name.ends_with(&lock_hash));

        let _guard = acquire_thread_create_lock(serenity::ChannelId::new(PARENT_ID), &normalized)
            .expect("Windows SID-scoped named mutex");
    }

    #[tokio::test]
    async fn active_thread_lookup_failure_never_posts() {
        let mut state = mock_state(serenity::ChannelType::Text);
        state.active_status = StatusCode::INTERNAL_SERVER_ERROR;
        let bodies = state.post_bodies.clone();
        let (http, server) = mock_http(state).await;

        let error = create(&http, "100", "must-not-create", None, &[], 1440)
            .await
            .expect_err("lookup failure must abort creation");
        server.abort();

        assert!(error.contains("creation was not attempted"));
        assert!(bodies.lock().unwrap().is_empty());
    }
}
