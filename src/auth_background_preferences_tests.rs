//! Uses only an owned disposable PostgreSQL 17 schema and synthetic image.
use super::*;
use crate::{mail_delivery, maps, router};
use axum::{
    body::{Body, to_bytes},
    http::{Method, Request},
};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{path::PathBuf, str::FromStr, sync::Arc, time::Duration as StdDuration};
use tokio::sync::Mutex;
use tower::ServiceExt;

fn fixture_state(db: PgPool, root: PathBuf) -> AppState {
    AppState {
        db,
        secret: Arc::new("background-preferences-fixture-secret-over-thirty-two-bytes".into()),
        internal_key: Arc::new(String::new()),
        origin: Arc::new("http://localhost:3000".into()),
        mail: Arc::new(mail_delivery::MailConfig::disabled()),
        secure_cookie: false,
        media_root: root.join("media"),
        maps_root: root.join("maps"),
        replay_root: root.join("replays"),
        client: reqwest::Client::new(),
        official: Arc::new(Mutex::new(maps::OfficialState::default())),
        limits: Default::default(),
        trusted_proxies: Default::default(),
        upload_slots: Arc::new(tokio::sync::Semaphore::new(2)),
        chat_runtime: Default::default(),
    }
}

async fn session(state: &AppState, id: i64) -> (Uuid, String) {
    let sid = Uuid::new_v4();
    let expiry = Utc::now() + Duration::hours(1);
    sqlx::query("INSERT INTO sessions(id,user_id,expires_at) VALUES($1,$2,$3)")
        .bind(sid)
        .bind(id)
        .bind(expiry)
        .execute(&state.db)
        .await
        .unwrap();
    let token = encode(
        &Header::default(),
        &Claims {
            sub: id,
            sid,
            exp: expiry.timestamp() as usize,
        },
        &EncodingKey::from_secret(state.secret.as_bytes()),
    )
    .unwrap();
    (sid, token)
}

async fn request(
    app: &axum::Router,
    method: Method,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = if let Some(body) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn stored(db: &PgPool, id: i64) -> Value {
    sqlx::query_scalar("SELECT preferences FROM users WHERE id=$1")
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn upload_background(app: &axum::Router, token: &str) -> (StatusCode, Value) {
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(16, 16)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let boundary = "background-preferences-synthetic-image";
    let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"kind\"\r\n\r\nbackground\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"synthetic.png\"\r\nContent-Type: image/png\r\n\r\n").into_bytes();
    body.extend_from_slice(&png.into_inner());
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/me/media")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
#[ignore = "requires ILLYA_BACKGROUND_PREFERENCES_TEST_DATABASE_URL pointing to an owned disposable PostgreSQL 17"]
async fn background_preferences_preserve_legacy_images_partial_updates_and_actor_authority() {
    let url = std::env::var("ILLYA_BACKGROUND_PREFERENCES_TEST_DATABASE_URL")
        .expect("Set the owned disposable background-preferences fixture URL");
    let control = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let version: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
        .fetch_one(&control)
        .await
        .unwrap();
    assert!(
        (170000..180000).contains(&version),
        "fixture must run PostgreSQL 17"
    );
    let suffix = Uuid::new_v4().simple().to_string();
    let schema = format!("illya_background_preferences_{suffix}");
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&control)
        .await
        .unwrap();
    let options = PgConnectOptions::from_str(&url)
        .unwrap()
        .options([("search_path", schema.as_str())])
        .application_name(&schema);
    let db = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .unwrap();
    let root = std::env::temp_dir().join(format!("illyaverse-background-preferences-{suffix}"));
    assert!(root.is_absolute() && root.parent() == Some(std::env::temp_dir().as_path()));
    let pool = db.clone();
    let application = schema.clone();
    let test_root = root.clone();
    let outcome = tokio::spawn(async move {
        sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
        let state = fixture_state(pool.clone(), test_root);
        let app = router(state.clone());
        let legacy = json!({
            "locale":"ja", "mode":"light", "accent_color":"#7dd8c5",
            "background_url":"/media/background/retained.webp", "background_blur":12,
            "background_dim":0.5, "panel_blur":11, "panel_opacity":0.625,
            "glass_level":45, "border_radius":20, "pm_friends_only":true
        });
        let id: i64 = sqlx::query_scalar("INSERT INTO users(username,email,password_hash,password_md5_hash,preferences) VALUES('BackgroundActor','background@example.test','synthetic','synthetic',$1) RETURNING id")
            .bind(&legacy).fetch_one(&pool).await.unwrap();
        let (_, token) = session(&state, id).await;

        let (status, value) = request(&app, Method::GET, "/api/v1/me/preferences", Some(&token), None).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["background_url"], legacy["background_url"]);
        assert!(value.get("background_mode").is_none());
        assert_eq!(value["background_color"], "#000000");
        assert_eq!(stored(&pool, id).await, legacy, "a legacy read must not backfill preferences");
        let (status, profile) = request(&app, Method::GET, "/api/v1/me", Some(&token), None).await;
        assert_eq!(status, StatusCode::OK, "{profile}");
        assert_eq!(profile["preferences"]["background_url"], legacy["background_url"]);
        assert!(profile["preferences"].get("background_mode").is_none());
        assert_eq!(stored(&pool, id).await, legacy);

        let (status, color) = request(&app, Method::PATCH, "/api/v1/me/preferences", Some(&token), Some(json!({"background_mode":"color","background_color":"#263544"}))).await;
        assert_eq!(status, StatusCode::OK, "{color}");
        assert_eq!(color["background_mode"], "color");
        assert_eq!(color["background_color"], "#263544");
        for (key, expected) in legacy.as_object().unwrap() {
            assert_eq!(&color[key], expected, "partial color patch changed {key}");
        }
        let (status, patched) = request(&app, Method::PATCH, "/api/v1/me/preferences", Some(&token), Some(json!({"locale":"en","panel_blur":17}))).await;
        assert_eq!(status, StatusCode::OK, "{patched}");
        assert_eq!(patched["background_mode"], "color");
        assert_eq!(patched["background_color"], "#263544");
        assert_eq!(patched["background_url"], legacy["background_url"]);
        assert_eq!(patched["locale"], "en");
        assert_eq!(patched["panel_blur"], 17);
        assert_eq!(patched["glass_level"], 45);
        assert_eq!(patched["pm_friends_only"], true);

        for patch in [
            json!({"background_mode":"solid","locale":"ja"}),
            json!({"background_mode":false,"locale":"ja"}),
            json!({"background_mode":null,"locale":"ja"}),
            json!({"background_color":"#fff","locale":"ja"}),
            json!({"background_color":"#12345678","locale":"ja"}),
            json!({"background_color":null,"locale":"ja"}),
            json!({"background_color":"#123456\n","locale":"ja"}),
            json!({"background_url":"https://remote.example/image.png","locale":"ja"}),
            json!({"background_colour":"#123456","locale":"ja"}),
        ] {
            let before = stored(&pool, id).await;
            let (status, error) = request(&app, Method::PATCH, "/api/v1/me/preferences", Some(&token), Some(patch.clone())).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{patch}: {error}");
            assert_eq!(stored(&pool, id).await, before, "invalid patch must commit no fields");
        }
        let before = stored(&pool, id).await;
        let (status, error) = request(&app, Method::PATCH, "/api/v1/me/preferences", None, Some(json!({"background_mode":"default"}))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{error}");
        assert_eq!(stored(&pool, id).await, before);

        let (status, upload) = upload_background(&app, &token).await;
        assert_eq!(status, StatusCode::OK, "{upload}");
        let image_url = upload["url"].as_str().unwrap();
        assert!(image_url.starts_with("/media/background/") && image_url.ends_with(".webp"));
        let after_upload = stored(&pool, id).await;
        assert_eq!(after_upload["background_mode"], "image");
        assert_eq!(after_upload["background_url"], image_url);
        for (key, expected) in before.as_object().unwrap() {
            if !matches!(key.as_str(), "background_mode" | "background_url") {
                assert_eq!(&after_upload[key], expected, "image upload changed {key}");
            }
        }
        let image_file = state.media_root.join(image_url.strip_prefix("/media/").unwrap());
        assert!(image_file.is_file());
        let image_bytes = tokio::fs::read(&image_file).await.unwrap();
        assert!(image::load_from_memory_with_format(&image_bytes, image::ImageFormat::WebP).is_ok());
        let (status, reset) = request(&app, Method::PATCH, "/api/v1/me/preferences", Some(&token), Some(json!({"background_mode":"default","background_color":"#000000","background_url":null}))).await;
        assert_eq!(status, StatusCode::OK, "{reset}");
        assert_eq!(reset["background_mode"], "default");
        assert_eq!(reset["background_color"], "#000000");
        assert_eq!(reset["background_url"], Value::Null);
        assert_eq!(reset["locale"], "en");
        assert_eq!(reset["panel_blur"], 17);
        assert_eq!(tokio::fs::read(&image_file).await.unwrap(), image_bytes, "reset must not delete an uploaded image");

        // An authenticated patch queued behind the actor lock must recheck a revoked session.
        let (queued_sid, queued_token) = session(&state, id).await;
        let before = stored(&pool, id).await;
        let mut blocking = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE").bind(id).fetch_one(&mut *blocking).await.unwrap();
        let queued_app = app.clone();
        let queued = tokio::spawn(async move {
            request(&queued_app, Method::PATCH, "/api/v1/me/preferences", Some(&queued_token), Some(json!({"background_mode":"color","background_color":"#ff0000"}))).await
        });
        tokio::time::timeout(StdDuration::from_secs(5), async {
            loop {
                let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND application_name=$1 AND wait_event_type='Lock' AND query LIKE '%ORDER BY id FOR NO KEY UPDATE%'")
                    .bind(&application).fetch_one(&pool).await.unwrap();
                if waiting > 0 { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("queued preferences patch must reach its actor lock");
        sqlx::query("DELETE FROM sessions WHERE id=$1").bind(queued_sid).execute(&mut *blocking).await.unwrap();
        blocking.commit().await.unwrap();
        let (status, error) = tokio::time::timeout(StdDuration::from_secs(5), queued).await.unwrap().unwrap();
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{error}");
        assert_eq!(stored(&pool, id).await, before, "revoked queued patch must not mutate preferences");
    }).await;
    db.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&control)
        .await
        .unwrap();
    control.close().await;
    if root.exists() {
        assert!(root.is_absolute() && root.parent() == Some(std::env::temp_dir().as_path()));
        tokio::fs::remove_dir_all(&root).await.unwrap();
    }
    outcome.unwrap();
}
