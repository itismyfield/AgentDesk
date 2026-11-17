use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::Notify;

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::test_support::wait::{WAIT_CAP, await_within};

const GUILD: u64 = 500;
const CATEGORY: u64 = 600;
const OTHER_CATEGORY: u64 = 601;
const ADMIN_ROLE: u64 = 700;

/// Fake Discord: guild channels as (id, kind, parent, name).
#[derive(Clone, Default)]
struct Discord {
    channels: Arc<Mutex<Vec<(u64, serenity::ChannelType, u64, String)>>>,
    lists: Arc<AtomicUsize>,
    posts: Arc<AtomicUsize>,
    admin: Arc<AtomicBool>,
    lose_create: Arc<AtomicBool>,
    channel_status: Arc<Mutex<Option<StatusCode>>>,
    hold_first_list: Option<(Arc<Notify>, Arc<Notify>)>,
}

impl Discord {
    fn add(&self, id: u64, kind: serenity::ChannelType, parent: u64, name: &str) {
        self.channels
            .lock()
            .unwrap()
            .push((id, kind, parent, name.into()));
    }
}

fn channel_json(
    &(id, kind, parent, ref name): &(u64, serenity::ChannelType, u64, String),
) -> Value {
    let mut channel = serenity::GuildChannel::default();
    channel.id = serenity::ChannelId::new(id);
    channel.guild_id = serenity::GuildId::new(GUILD);
    channel.name = name.clone();
    channel.kind = kind;
    channel.parent_id = Some(serenity::ChannelId::new(parent));
    serde_json::to_value(channel).unwrap()
}

fn failure(status: StatusCode) -> Response {
    (status, Json(json!({"message": "fake failure", "code": 0}))).into_response()
}

async fn list(State(d): State<Discord>) -> Json<Value> {
    // The snapshot is taken on arrival, so a held caller answers from what it saw then.
    let snapshot: Vec<Value> = d
        .channels
        .lock()
        .unwrap()
        .iter()
        .map(channel_json)
        .collect();
    if d.lists.fetch_add(1, SeqCst) == 0
        && let Some((entered, release)) = &d.hold_first_list
    {
        entered.notify_one();
        release.notified().await;
    }
    Json(Value::Array(snapshot))
}

async fn create(State(d): State<Discord>, Json(body): Json<Value>) -> Response {
    let number = d.posts.fetch_add(1, SeqCst) as u64;
    let parent = body["parent_id"].as_str().unwrap().parse().unwrap();
    let created = (
        900 + number,
        serenity::ChannelType::Forum,
        parent,
        body["name"].as_str().unwrap().into(),
    );
    if d.lose_create.load(SeqCst) {
        return failure(StatusCode::BAD_GATEWAY);
    }
    d.channels.lock().unwrap().push(created.clone());
    Json(channel_json(&created)).into_response()
}

async fn channel(State(d): State<Discord>, Path(id): Path<u64>) -> Response {
    if let Some(status) = *d.channel_status.lock().unwrap() {
        return failure(status);
    }
    match d.channels.lock().unwrap().iter().find(|c| c.0 == id) {
        Some(found) => Json(channel_json(found)).into_response(),
        None => failure(StatusCode::NOT_FOUND),
    }
}

async fn fake_discord(d: Discord) -> serenity::Http {
    let mut me = serenity::User::default();
    me.id = serenity::UserId::new(1);
    let mut member = serde_json::to_value(serenity::Member::default()).unwrap();
    member["roles"] = json!([ADMIN_ROLE.to_string()]);
    let admin = d.admin.clone();
    let roles = move || {
        let mut role = serenity::Role::default();
        role.id = serenity::RoleId::new(ADMIN_ROLE);
        if admin.load(SeqCst) {
            role.permissions = serenity::Permissions::ADMINISTRATOR;
        }
        Json(json!([role]))
    };
    let app = Router::new()
        .route("/api/v10/guilds/{g}/channels", get(list).post(create))
        .route("/api/v10/channels/{id}", get(channel))
        .route("/api/v10/users/@me", get(move || async move { Json(me) }))
        .route(
            "/api/v10/guilds/{g}/members/{u}",
            get(move || async move { Json(member) }),
        )
        .route(
            "/api/v10/guilds/{g}/roles",
            get(move || async move { roles() }),
        )
        .with_state(d);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    serenity::HttpBuilder::new("test-token")
        .proxy(proxy)
        .ratelimiter_disabled(true)
        .build()
}

async fn resolve(
    pool: &PgPool,
    http: &serenity::Http,
    config: &DecisionsConfig,
) -> Result<ResolvedForum, ForumError> {
    let (guild, category) = (
        serenity::GuildId::new(GUILD),
        serenity::ChannelId::new(CATEGORY),
    );
    resolve_or_create(pool, http, config, guild, category, "decision", None).await
}

async fn pg_with(d: &Discord) -> (TestPostgresDb, PgPool, Arc<serenity::Http>) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    (db, pool, Arc::new(fake_discord(d.clone()).await))
}

#[test]
fn channel_create_forum_kind_requires_category() {
    use crate::cli::args::{Commands, DiscordAction};
    use crate::cli::discord_channel_create::ChannelKind;
    #[derive(clap::Parser)]
    struct Probe {
        #[command(subcommand)]
        command: Commands,
    }
    let parse = |extra: &[&str]| {
        let base = [
            "agentdesk",
            "discord",
            "channel-create",
            "--name",
            "decision",
        ];
        let parsed = <Probe as clap::Parser>::try_parse_from(base.iter().chain(extra));
        parsed.map(|probe| match probe.command {
            Commands::Discord {
                action: DiscordAction::ChannelCreate { kind, .. },
            } => kind,
            _ => panic!("not channel-create"),
        })
    };
    assert_eq!(parse(&[]).unwrap(), ChannelKind::Text);
    assert!(parse(&["--kind", "forum"]).is_err());
    let forum = parse(&["--kind", "forum", "--category-id", "6"]);
    assert_eq!(forum.unwrap(), ChannelKind::Forum);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decision_forum_pg_concurrent_callers_create_one_forum() {
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let d = Discord {
        hold_first_list: Some((entered.clone(), release.clone())),
        ..Default::default()
    };
    d.admin.store(true, SeqCst);
    let (db, pool, http) = pg_with(&d).await;
    let config = DecisionsConfig::default();
    let call = || {
        let (pool, http, config) = (pool.clone(), http.clone(), config.clone());
        tokio::spawn(async move { resolve(&pool, &http, &config).await })
    };
    let first = call();
    await_within(entered.notified(), WAIT_CAP).await;
    let second = call();
    // The second caller either waits on the category lock or, without it, lists channels too.
    await_within(
        async {
            let waiting =
                "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted
                 AND database = (SELECT oid FROM pg_database WHERE datname = current_database())";
            while d.lists.load(SeqCst) < 2
                && sqlx::query_scalar::<_, i64>(waiting)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
                    == 0
            {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        },
        WAIT_CAP,
    )
    .await;
    release.notify_one();
    let first = await_within(first, WAIT_CAP).await.unwrap().unwrap();
    let second = await_within(second, WAIT_CAP).await.unwrap().unwrap();
    assert_eq!(
        d.posts.load(SeqCst),
        1,
        "one create for two concurrent callers"
    );
    assert_eq!(first.id, second.id);
    assert_eq!([first.created, second.created], [true, false]);
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decision_forum_pg_unknown_create_is_never_resent() {
    let d = Discord::default();
    d.admin.store(true, SeqCst);
    d.lose_create.store(true, SeqCst);
    let (db, pool, http) = pg_with(&d).await;
    let config = DecisionsConfig::default();
    for _ in 0..2 {
        let error = resolve(&pool, &http, &config).await.unwrap_err();
        assert!(matches!(error, ForumError::CreateUnknown), "{error}");
    }
    assert_eq!(
        d.posts.load(SeqCst),
        1,
        "a create with an unknown result is not sent again"
    );
    // The lost create lands late: the next call adopts it instead of creating another.
    d.add(950, serenity::ChannelType::Forum, CATEGORY, "decision");
    let adopted = resolve(&pool, &http, &config).await.unwrap();
    assert_eq!(
        (adopted.id, adopted.created, d.posts.load(SeqCst)),
        (950, false, 1)
    );
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decision_forum_pg_reuses_existing_and_rechecks_the_cache() {
    let d = Discord::default();
    d.add(10, serenity::ChannelType::Text, CATEGORY, "decision");
    d.add(11, serenity::ChannelType::Forum, OTHER_CATEGORY, "decision");
    let (db, pool, http) = pg_with(&d).await;
    let config = DecisionsConfig::default();
    let error = resolve(&pool, &http, &config).await.unwrap_err();
    assert!(matches!(error, ForumError::InventoryUnavailable), "{error}");

    d.add(12, serenity::ChannelType::Forum, CATEGORY, "decision");
    let found = resolve(&pool, &http, &config).await.unwrap();
    assert_eq!((found.id, found.created), (12, false));
    let lists = d.lists.load(SeqCst);
    assert_eq!(resolve(&pool, &http, &config).await.unwrap(), found);
    assert_eq!(
        d.lists.load(SeqCst),
        lists,
        "a valid cached forum needs no channel list"
    );

    *d.channel_status.lock().unwrap() = Some(StatusCode::FORBIDDEN);
    let error = resolve(&pool, &http, &config).await.unwrap_err();
    assert!(matches!(error, ForumError::Discord(_)), "{error}");
    *d.channel_status.lock().unwrap() = None;

    d.channels
        .lock()
        .unwrap()
        .iter_mut()
        .find(|c| c.0 == 12)
        .unwrap()
        .2 = OTHER_CATEGORY;
    d.admin.store(true, SeqCst);
    let replaced = resolve(&pool, &http, &config).await.unwrap();
    assert_eq!((replaced.created, d.posts.load(SeqCst)), (true, 1));
    assert_ne!(replaced.id, 12);

    d.channels.lock().unwrap().retain(|c| c.0 != replaced.id);
    d.add(13, serenity::ChannelType::Forum, CATEGORY, "decision");
    d.add(14, serenity::ChannelType::Forum, CATEGORY, "decision");
    let error = resolve(&pool, &http, &config).await.unwrap_err();
    assert!(matches!(error, ForumError::Ambiguous(2, _)), "{error}");

    let mut config = DecisionsConfig::default();
    config
        .forums_by_category
        .insert(CATEGORY.to_string(), "13".into());
    assert_eq!(resolve(&pool, &http, &config).await.unwrap().id, 13);
    config
        .forums_by_category
        .insert(CATEGORY.to_string(), "10".into());
    let error = resolve(&pool, &http, &config).await.unwrap_err();
    assert!(matches!(error, ForumError::OverrideInvalid(_)), "{error}");
    db.drop().await;
}
