//! Transient archive fixtures own their PostgreSQL schema and loopback providers.
use crate::{
    AppState, Router, mail_delivery,
    maps::{
        OfficialState,
        map_mutation_tests::{MutationFixture, SERVICE_KEY, actor, file_count, osu_bytes, request},
    },
    native_map_tests::OfficialServer,
    router,
};
use axum::{
    body::{Body, Bytes, to_bytes},
    extract::{Path, State},
    http::{HeaderMap, Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use std::{
    collections::HashMap,
    io::{Cursor, Write},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Notify};
use tower::ServiceExt;

const INTERNAL: &str = "/api/v1/internal/beatmapsets/archive";

struct ArchiveSet {
    id: i64,
    charts: Vec<Vec<u8>>,
    metadata: Value,
    bytes: Vec<u8>,
}
impl ArchiveSet {
    fn new(id: i64) -> Self {
        let charts = ["Easy", "Hard"]
            .into_iter()
            .enumerate()
            .map(|(n, version)| {
                String::from_utf8(osu_bytes(&format!("Transient archive {id}")))
                    .unwrap()
                    .replace("Mode:0", "Mode:0\nAudioFilename:assets/audio.mp3")
                    .replace(
                        "Version:Easy",
                        &format!(
                            "Version:{version}\nBeatmapID:{}\nBeatmapSetID:{id}",
                            id * 10 + n as i64 + 1
                        ),
                    )
                    .into_bytes()
            })
            .collect::<Vec<_>>();
        let beatmaps = charts.iter().enumerate().map(|(n, bytes)| json!({
            "id":id*10+n as i64+1,"beatmapset_id":id,"checksum":format!("{:x}",md5::compute(bytes)),
            "version":(["Easy","Hard"][n]),"mode_int":0,"mode":"osu","status":"ranked",
            "last_updated":"2020-02-02T03:04:05Z","difficulty_rating":1.0+n as f64,
            "bpm":120.0,"total_length":2,"hit_length":1,"max_combo":2,"ar":5.0,"cs":4.0,
            "accuracy":5.0,"drain":5.0,"count_circles":2,"count_sliders":0,"count_spinners":0
        })).collect::<Vec<_>>();
        let metadata = json!({"id":id,"artist":"Illyaverse","artist_unicode":"","title":"Transient archive",
            "title_unicode":"","creator":"Owned fixture","status":"ranked","bpm":120.0,
            "source":"","tags":"owned archive fixture","nsfw":false,"availability":{"download_disabled":false},
            "submitted_date":"2020-01-02T03:04:05Z","last_updated":"2020-02-02T03:04:05Z","ranked_date":null,
            "genre":{"id":5,"name":"Video Game"},"language":{"id":2,"name":"English"},"beatmaps":beatmaps,
            "converts":[{"id":1999999000,"convert":true}]});
        let bytes = archive(&charts, Some(b"owned synthetic audio asset"));
        Self {
            id,
            charts,
            metadata,
            bytes,
        }
    }
}
fn archive(charts: &[Vec<u8>], audio: Option<&[u8]>) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (n, chart) in charts.iter().enumerate().rev() {
        writer
            .start_file(format!("Original difficulty {n}.osu"), options)
            .unwrap();
        writer.write_all(chart).unwrap();
    }
    if let Some(audio) = audio {
        writer.start_file("assets/audio.mp3", options).unwrap();
        writer.write_all(audio).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[derive(Default)]
struct Pause {
    entered: Notify,
    released: Notify,
}
impl Pause {
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .expect("owned archive provider was reached");
    }
    fn release(&self) {
        self.released.notify_one();
    }
}
#[derive(Clone)]
enum ProviderReply {
    Archive(Vec<u8>),
    RateLimit(String),
    Redirect(String),
    Oversize,
    Unavailable,
}
#[derive(Clone, Default)]
struct ProviderState {
    replies: Arc<Mutex<HashMap<i64, ProviderReply>>>,
    pause: Arc<Mutex<Option<Arc<Pause>>>>,
    requests: Arc<AtomicUsize>,
}
struct Provider {
    template: String,
    state: ProviderState,
    task: tokio::task::JoinHandle<()>,
}
impl Provider {
    async fn start() -> Self {
        let state = ProviderState::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let template = format!(
            "http://{}/archive/{{set_id}}",
            listener.local_addr().unwrap()
        );
        let app = Router::new()
            .route("/archive/{id}", get(provider_reply))
            .with_state(state.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            template,
            state,
            task,
        }
    }
    async fn add(&self, set: &ArchiveSet) {
        self.reply(set.id, ProviderReply::Archive(set.bytes.clone()))
            .await;
    }
    async fn reply(&self, id: i64, reply: ProviderReply) {
        self.state.replies.lock().await.insert(id, reply);
    }
    async fn pause_next(&self) -> Arc<Pause> {
        let pause = Arc::new(Pause::default());
        *self.state.pause.lock().await = Some(pause.clone());
        pause
    }
    fn count(&self) -> usize {
        self.state.requests.load(Ordering::SeqCst)
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn provider_reply(
    State(state): State<ProviderState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    state.requests.fetch_add(1, Ordering::SeqCst);
    assert!(
        headers.get(header::AUTHORIZATION).is_none(),
        "never forward official or player credentials to provider"
    );
    assert!(
        headers.get(header::COOKIE).is_none(),
        "never forward a player cookie to provider"
    );
    assert!(headers.get("x-illya-service-key").is_none());
    assert_eq!(headers[header::ACCEPT_ENCODING], "identity");
    let reply = state.replies.lock().await.get(&id).cloned();
    let pause = state.pause.lock().await.take();
    if let Some(pause) = pause {
        pause.entered.notify_one();
        pause.released.notified().await;
    }
    match reply {
        Some(ProviderReply::Archive(bytes)) => {
            ([(header::CONTENT_TYPE, "application/zip")], bytes).into_response()
        }
        Some(ProviderReply::RateLimit(retry)) => (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, retry)],
        )
            .into_response(),
        Some(ProviderReply::Redirect(location)) => {
            (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
        }
        Some(ProviderReply::Oversize) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, "67108865")
            .body(Body::from_stream(futures_util::stream::once(async {
                Ok::<_, std::io::Error>(Bytes::from_static(b"x"))
            })))
            .unwrap(),
        Some(ProviderReply::Unavailable) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
async fn configure(state: &AppState, provider: &Provider) {
    state
        .official
        .lock()
        .await
        .fixture_archive_provider(provider.template.clone());
}
async fn raw(
    app: &Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    key: Option<&str>,
    body: Vec<u8>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    if let Some(key) = key {
        builder = builder.header("x-illya-service-key", key);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 66 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}
async fn public(app: &Router, id: i64, cookie: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
    raw(
        app,
        "GET",
        &format!("/api/v1/official/beatmapsets/{id}/archive"),
        cookie,
        None,
        Vec::new(),
    )
    .await
}
async fn internal(app: &Router, id: i64, user: Option<i64>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let body = match user {
        Some(user) => json!({"official_set_id":id,"user_id":user}),
        None => json!({"official_set_id":id}),
    };
    raw(
        app,
        "POST",
        INTERNAL,
        None,
        Some(SERVICE_KEY),
        body.to_string().into_bytes(),
    )
    .await
}
fn assert_archive(response: (StatusCode, HeaderMap, Vec<u8>), set: &ArchiveSet) {
    assert_eq!(
        response.0,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&response.2)
    );
    assert_eq!(
        response.1[header::CONTENT_TYPE],
        "application/x-osu-archive"
    );
    assert_eq!(
        response.1[header::CONTENT_DISPOSITION],
        format!(
            "attachment; filename=\"illyaverse-official-{}.osz\"",
            set.id
        )
    );
    assert_eq!(response.1[header::CACHE_CONTROL], "private, no-store");
    assert_eq!(response.1["x-content-type-options"], "nosniff");
    assert_eq!(
        response.1["x-illya-archive-sha256"],
        format!("{:x}", Sha256::digest(&set.bytes))
    );
    assert_eq!(
        response.2, set.bytes,
        "archive is the exact complete provider package, never reconstructed .osu files"
    );
}
fn error(response: &(StatusCode, HeaderMap, Vec<u8>)) -> Value {
    serde_json::from_slice(&response.2).unwrap()
}
async fn catalogue(state: &AppState) -> Value {
    let mut snapshot = serde_json::Map::new();
    for (table, order) in [
        ("beatmaps", "id"),
        ("beatmap_packages", "id"),
        ("official_beatmapset_archives", "official_set_id"),
        (
            "official_beatmapset_archive_members",
            "archive_id,beatmap_id",
        ),
        ("beatmap_filename_aliases", "filename,beatmap_id"),
        ("audit_events", "id"),
        ("lazer_beatmap_updates", "id"),
        ("scores", "id"),
    ] {
        let rows: Value = sqlx::query_scalar(&format!(
            "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY {order}),'[]'::jsonb) FROM {table} t"
        ))
        .fetch_one(&state.db)
        .await
        .unwrap();
        snapshot.insert(table.into(), rows);
    }
    Value::Object(snapshot)
}
async fn unchanged(state: &AppState, before: &Value) {
    assert_eq!(
        &catalogue(state).await,
        before,
        "transient downloads must not publish catalogue, score, audit or cursor writes"
    );
    assert_eq!(file_count(&state.maps_root).await, 0);
    assert_eq!(file_count(&state.replay_root).await, 0);
    assert_eq!(file_count(&state.media_root).await, 0);
}

async fn file_snapshot(root: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut files = std::collections::BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut entries = tokio::fs::read_dir(directory).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let kind = entry.file_type().await.unwrap();
            assert!(
                !kind.is_symlink(),
                "owned imported fixture has regular assets"
            );
            if kind.is_dir() {
                pending.push(entry.path());
            } else {
                let path = entry.path();
                let key = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                assert!(
                    files
                        .insert(key, tokio::fs::read(path).await.unwrap())
                        .is_none()
                );
            }
        }
    }
    files
}

#[tokio::test]
async fn official_archive_download_guards_reject_before_lazy_database_access() {
    let state = AppState {
        db: PgPoolOptions::new()
            .acquire_timeout(Duration::from_secs(5))
            .connect_lazy("postgres://fixture@127.0.0.1:1/fixture")
            .unwrap(),
        secret: Arc::new("archive-fixture-session-key-at-least-thirty-two-bytes".into()),
        internal_key: Arc::new(SERVICE_KEY.into()),
        origin: Arc::new("http://localhost:3000".into()),
        mail: Arc::new(mail_delivery::MailConfig::disabled()),
        secure_cookie: false,
        maps_root: std::env::temp_dir(),
        media_root: std::env::temp_dir(),
        replay_root: std::env::temp_dir(),
        client: reqwest::Client::new(),
        official: Arc::new(Mutex::new(OfficialState::default())),
        limits: Default::default(),
        trusted_proxies: Default::default(),
        upload_slots: Arc::new(tokio::sync::Semaphore::new(2)),
        chat_runtime: Default::default(),
    };
    let app = router(state);
    for key in [None, Some("wrong")] {
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(1),
                raw(&app, "POST", INTERNAL, None, key, b"not JSON".to_vec())
            )
            .await
            .unwrap()
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    for body in [
        "{}".to_owned(),
        "{\"official_set_id\":42,\"official_set_id\":43}".into(),
        json!({"official_set_id":0}).to_string(),
        json!({"official_set_id":"42"}).to_string(),
        json!({"official_set_id":42,"user_id":null}).to_string(),
        json!({"official_set_id":42,"user_id":0}).to_string(),
        json!({"official_set_id":42,"user_id":-1}).to_string(),
        json!({"official_set_id":42,"url":"http://127.0.0.1/"}).to_string(),
        json!({"official_set_id":2_000_000_000_i64}).to_string(),
        " ".repeat(4097),
    ] {
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            raw(
                &app,
                "POST",
                INTERNAL,
                None,
                Some(SERVICE_KEY),
                body.into_bytes(),
            ),
        )
        .await
        .unwrap();
        assert!(matches!(
            response.0,
            StatusCode::BAD_REQUEST | StatusCode::CONFLICT | StatusCode::PAYLOAD_TOO_LARGE
        ));
    }
    for id in [0, -1, 2_000_000_000] {
        let response = tokio::time::timeout(Duration::from_secs(1), public(&app, id, None))
            .await
            .unwrap();
        assert!(matches!(
            response.0,
            StatusCode::BAD_REQUEST | StatusCode::CONFLICT
        ));
    }
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(1),
            public(&app, 42, Some("illya_session=malformed"))
        )
        .await
        .unwrap()
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn official_archive_download_exact_bytes_cache_singleflight_and_read_only() {
    let official = OfficialServer::start().await;
    let provider = Provider::start().await;
    let set = ArchiveSet::new(9101);
    official.add_set(set.id, set.metadata.clone()).await;
    provider.add(&set).await;
    let fixture = MutationFixture::new("download_exact", official.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        configure(&state, &provider).await;
        let (user, _, _, cookie) = actor(&state, "ArchiveExact", &[]).await;
        let before = catalogue(&state).await;
        let pause = provider.pause_next().await;
        let first_app = app.clone();
        let id = set.id;
        let first = tokio::spawn(async move { public(&first_app, id, None).await });
        pause.entered().await;
        let second_app = app.clone();
        let second_cookie = cookie.clone();
        let mut second =
            tokio::spawn(async move { public(&second_app, id, Some(&second_cookie)).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut second)
                .await
                .is_err(),
            "the second request must wait for the same in-flight provider response"
        );
        assert_eq!(
            provider.count(),
            1,
            "a paused same-set request cannot start a second provider download"
        );
        pause.release();
        assert_archive(first.await.unwrap(), &set);
        assert_archive(second.await.unwrap(), &set);
        assert_archive(public(&app, id, None).await, &set);
        assert_archive(internal(&app, id, Some(user)).await, &set);
        assert_eq!(
            provider.count(),
            1,
            "concurrent public and internal callers share one transient package"
        );
        assert_eq!(
            official.set_queries().await.len(),
            8,
            "every request refreshes policy before and after the cache/provider"
        );
        assert_eq!(
            official.counters()[1..],
            [0, 0, 0],
            "an archive read does not resolve or fetch individual .osu files"
        );
        unchanged(&state, &before).await;
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn official_archive_download_rejects_chart_only_incomplete_empty_audio_and_crc() {
    let official = OfficialServer::start().await;
    let provider = Provider::start().await;
    let fixture = MutationFixture::new("download_invalid", official.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        configure(&state, &provider).await;
        let before = catalogue(&state).await;
        for (index, kind) in [
            "chart-only",
            "incomplete",
            "empty-audio",
            "audio-crc",
            "wrong-id",
            "wrong-mode",
        ]
        .into_iter()
        .enumerate()
        {
            let set = ArchiveSet::new(9120 + index as i64);
            official.add_set(set.id, set.metadata.clone()).await;
            let bytes = match kind {
                "chart-only" => archive(&set.charts, None),
                "incomplete" => archive(&set.charts[..1], Some(b"owned synthetic audio asset")),
                "empty-audio" => archive(&set.charts, Some(b"")),
                "audio-crc" => {
                    let mut bytes = set.bytes.clone();
                    let marker = b"owned synthetic audio asset";
                    let at = bytes
                        .windows(marker.len())
                        .position(|w| w == marker)
                        .unwrap();
                    bytes[at] ^= 1;
                    bytes
                }
                "wrong-id" => {
                    let charts = set
                        .charts
                        .iter()
                        .map(|b| {
                            String::from_utf8(b.clone())
                                .unwrap()
                                .replace(&format!("BeatmapSetID:{}", set.id), "BeatmapSetID:99999")
                                .into_bytes()
                        })
                        .collect::<Vec<_>>();
                    archive(&charts, Some(b"owned synthetic audio asset"))
                }
                "wrong-mode" => {
                    let charts = set
                        .charts
                        .iter()
                        .map(|b| {
                            String::from_utf8(b.clone())
                                .unwrap()
                                .replace("Mode:0", "Mode:1")
                                .into_bytes()
                        })
                        .collect::<Vec<_>>();
                    archive(&charts, Some(b"owned synthetic audio asset"))
                }
                _ => unreachable!(),
            };
            provider.reply(set.id, ProviderReply::Archive(bytes)).await;
            let response = public(&app, set.id, None).await;
            assert_eq!(
                response.0,
                StatusCode::BAD_GATEWAY,
                "{kind}: {}",
                error(&response)
            );
            assert_eq!(
                error(&response)["error"]["code"],
                "official_archive_invalid",
                "{kind}"
            );
        }
        assert_eq!(provider.count(), 6);
        unchanged(&state, &before).await;
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn official_archive_download_disabled_unknown_and_invalid_cookie_never_fetch() {
    let official = OfficialServer::start().await;
    let provider = Provider::start().await;
    let fixture = MutationFixture::new("download_policy", official.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        configure(&state, &provider).await;
        let before = catalogue(&state).await;
        for (index, policy) in [Some(true), None].into_iter().enumerate() {
            let set = ArchiveSet::new(9140 + index as i64);
            let mut metadata = set.metadata.clone();
            match policy {
                Some(value) => metadata["availability"]["download_disabled"] = json!(value),
                None => {
                    metadata.as_object_mut().unwrap().remove("availability");
                }
            }
            official.add_set(set.id, metadata).await;
            provider.add(&set).await;
            let response = public(&app, set.id, None).await;
            assert_eq!(response.0, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(
                error(&response)["error"]["code"],
                "official_archive_unavailable"
            );
        }
        assert_eq!(
            public(&app, 9140, Some("illya_session=malformed")).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(provider.count(), 0);
        unchanged(&state, &before).await;
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn official_archive_download_policy_and_manifest_change_during_provider_wait() {
    let official = OfficialServer::start().await;
    let provider = Provider::start().await;
    let fixture = MutationFixture::new("download_changed", official.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        configure(&state, &provider).await;
        let before = catalogue(&state).await;
        for (index, kind) in ["disabled", "checksum"].into_iter().enumerate() {
            let set = ArchiveSet::new(9160 + index as i64);
            official.add_set(set.id, set.metadata.clone()).await;
            provider.add(&set).await;
            let pause = provider.pause_next().await;
            let worker_app = app.clone();
            let id = set.id;
            let pending = tokio::spawn(async move { public(&worker_app, id, None).await });
            pause.entered().await;
            let mut changed = set.metadata.clone();
            if kind == "disabled" {
                changed["availability"]["download_disabled"] = json!(true);
            } else {
                changed["beatmaps"][0]["checksum"] = json!("f".repeat(32));
            }
            official.add_set(id, changed).await;
            pause.release();
            let response = pending.await.unwrap();
            assert_eq!(
                response.0,
                if kind == "disabled" {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::BAD_GATEWAY
                },
                "{kind}: {}",
                error(&response)
            );
            official.add_set(id, set.metadata.clone()).await;
            if kind == "checksum" {
                assert_eq!(
                    public(&app, id, None).await.0,
                    StatusCode::BAD_GATEWAY,
                    "an invalid archive remains negatively cached after metadata restoration"
                );
                state
                    .official
                    .lock()
                    .await
                    .expire_archive_download_failure(id);
            }
            assert_archive(public(&app, id, None).await, &set);
        }
        assert_eq!(
            provider.count(),
            4,
            "a policy-invalid or manifest-invalid response must never enter the positive cache"
        );
        unchanged(&state, &before).await;
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn official_archive_download_cache_hit_refreshes_policy_and_original_checksums() {
    let official = OfficialServer::start().await;
    let provider = Provider::start().await;
    let set = ArchiveSet::new(9181);
    official.add_set(set.id, set.metadata.clone()).await;
    provider.add(&set).await;
    let fixture = MutationFixture::new("download_cache_policy", official.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        configure(&state, &provider).await;
        let before = catalogue(&state).await;
        assert_archive(public(&app, set.id, None).await, &set);
        let mut disabled = set.metadata.clone();
        disabled["availability"]["download_disabled"] = json!(true);
        official.add_set(set.id, disabled).await;
        assert_eq!(
            public(&app, set.id, None).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let mut changed = set.metadata.clone();
        changed["beatmaps"][1]["checksum"] = json!("e".repeat(32));
        official.add_set(set.id, changed).await;
        let response = public(&app, set.id, None).await;
        assert_eq!(response.0, StatusCode::BAD_GATEWAY);
        assert_eq!(
            error(&response)["error"]["code"],
            "official_archive_invalid"
        );
        official.add_set(set.id, set.metadata.clone()).await;
        let retry = public(&app, set.id, None).await;
        assert_eq!(
            retry.0,
            StatusCode::BAD_GATEWAY,
            "restoring metadata does not bypass the bounded negative cache"
        );
        assert_eq!(
            provider.count(),
            1,
            "a negatively cached invalid package does not reach the provider again"
        );
        state
            .official
            .lock()
            .await
            .expire_archive_download_failure(set.id);
        assert_archive(public(&app, set.id, None).await, &set);
        assert_eq!(
            provider.count(),
            2,
            "after negative-cache expiry, the provider package must be acquired and verified again"
        );
        unchanged(&state, &before).await;
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn official_archive_download_rechecks_restriction_session_and_withdrawal_after_pause() {
    let official = OfficialServer::start().await;
    let provider = Provider::start().await;
    let fixture = MutationFixture::new("download_auth", official.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        configure(&state, &provider).await;
        for (index, change) in ["restriction", "session", "withdrawal"].into_iter().enumerate() {
            let set = ArchiveSet::new(9200 + index as i64);
            official.add_set(set.id, set.metadata.clone()).await;
            provider.add(&set).await;
            let (user, _, sid, cookie) = actor(&state, &format!("ArchiveAuth{index}"), &[]).await;
            let before_intervention = catalogue(&state).await;
            let pause = provider.pause_next().await;
            let worker_app = app.clone();
            let id = set.id;
            let pending = tokio::spawn(async move { public(&worker_app, id, Some(&cookie)).await });
            pause.entered().await;
            assert_eq!(catalogue(&state).await, before_intervention, "a pending download makes no provisional catalogue writes");
            match change {
                "restriction" => {
                    sqlx::query("UPDATE users SET is_restricted=true WHERE id=$1").bind(user).execute(&state.db).await.unwrap();
                }
                "session" => {
                    sqlx::query("DELETE FROM sessions WHERE id=$1").bind(sid).execute(&state.db).await.unwrap();
                }
                "withdrawal" => {
                    sqlx::query("INSERT INTO beatmapset_availability(native_set_id,withdrawn,revision,reason,actor_id) VALUES($1,true,1,'Owned provider wait fixture',$2)").bind(id).bind(user).execute(&state.db).await.unwrap();
                }
                _ => unreachable!(),
            }
            let after_intervention = catalogue(&state).await;
            if change == "withdrawal" {
                let before_events = before_intervention["lazer_beatmap_updates"].as_array().unwrap();
                let after_events = after_intervention["lazer_beatmap_updates"].as_array().unwrap();
                assert_eq!(after_events.len(), before_events.len() + 1, "the intentional withdrawal publishes exactly one native cursor event");
                assert_eq!(&after_events[..before_events.len()], before_events.as_slice());
                assert_eq!(after_events.last().unwrap()["beatmap_set_id"], id);
                let mut remaining = after_intervention.clone();
                remaining["lazer_beatmap_updates"] = before_intervention["lazer_beatmap_updates"].clone();
                assert_eq!(remaining, before_intervention, "the fixture withdrawal makes no other catalogue mutations");
            } else {
                assert_eq!(after_intervention, before_intervention);
            }
            pause.release();
            let response = pending.await.unwrap();
            assert_eq!(response.0, if change == "withdrawal" { StatusCode::GONE } else { StatusCode::UNAUTHORIZED }, "{change}: {}", error(&response));
            if change == "restriction" {
                assert_eq!(internal(&app, id, Some(user)).await.0, StatusCode::UNAUTHORIZED);
            }
            unchanged(&state, &after_intervention).await;
        }
        assert_eq!(provider.count(), 3);
    }).await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn official_archive_download_provider_429_is_negative_cached_and_globally_bounded() {
    let official = OfficialServer::start().await;
    let provider = Provider::start().await;
    let first = ArchiveSet::new(9221);
    let second = ArchiveSet::new(9222);
    for set in [&first, &second] {
        official.add_set(set.id, set.metadata.clone()).await;
        provider.add(set).await;
    }
    provider
        .reply(first.id, ProviderReply::RateLimit("0".into()))
        .await;
    let fixture = MutationFixture::new("download_429", official.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        configure(&state, &provider).await;
        let before = catalogue(&state).await;
        for id in [first.id, first.id, second.id] {
            let response = public(&app, id, None).await;
            assert_eq!(response.0, StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(
                error(&response)["error"]["code"],
                "official_archive_rate_limited"
            );
        }
        assert_eq!(
            provider.count(),
            1,
            "a zero Retry-After cannot spin and another set respects global backoff"
        );
        provider.add(&first).await;
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_archive(public(&app, first.id, None).await, &first);
        assert_eq!(
            provider.count(),
            2,
            "the clamped short negative entry expires"
        );
        unchanged(&state, &before).await;
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn official_archive_download_redirect_cannot_leave_provider_and_oversize_is_rejected() {
    let official = OfficialServer::start().await;
    let provider = Provider::start().await;
    let outsider = Provider::start().await;
    let redirected = ArchiveSet::new(9241);
    let oversized = ArchiveSet::new(9242);
    for set in [&redirected, &oversized] {
        official.add_set(set.id, set.metadata.clone()).await;
        outsider.add(set).await;
    }
    provider
        .reply(
            redirected.id,
            ProviderReply::Redirect(
                outsider
                    .template
                    .replace("{set_id}", &redirected.id.to_string()),
            ),
        )
        .await;
    provider.reply(oversized.id, ProviderReply::Oversize).await;
    let fixture = MutationFixture::new("download_redirect", official.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        configure(&state, &provider).await;
        let before = catalogue(&state).await;
        for id in [redirected.id, oversized.id] {
            let response = public(&app, id, None).await;
            assert_eq!(response.0, StatusCode::BAD_GATEWAY);
            assert_eq!(
                error(&response)["error"]["code"],
                "official_archive_provider_failed"
            );
        }
        assert_eq!(provider.count(), 2);
        assert_eq!(
            outsider.count(),
            0,
            "an unapproved redirect target is never contacted"
        );
        unchanged(&state, &before).await;
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn official_archive_download_stored_version_conflict_precedes_provider_access() {
    let official = OfficialServer::start().await;
    let provider = Provider::start().await;
    let set = ArchiveSet::new(9261);
    official.add_set(set.id, set.metadata.clone()).await;
    provider.add(&set).await;
    let fixture = MutationFixture::new("download_version", official.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result=tokio::spawn(async move {
        configure(&state,&provider).await;
        sqlx::query("INSERT INTO beatmaps(official_id,official_set_id,title,artist,creator,version,mode,stars,md5,osu_path,source) VALUES($1,$2,'Stored version','Fixture','Fixture','Easy',0,1,$3,'owned-existing.osu','official')")
            .bind(set.id*10+1).bind(set.id).bind("0".repeat(32)).execute(&state.db).await.unwrap();
        let before=catalogue(&state).await;let response=public(&app,set.id,None).await;
        assert_eq!(response.0,StatusCode::CONFLICT);assert_eq!(error(&response)["error"]["code"],"official_set_version_conflict");
        assert_eq!(provider.count(),0);unchanged(&state,&before).await;
    }).await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn official_archive_download_default_configuration_is_unavailable_without_network() {
    let official = OfficialServer::start().await;
    let set = ArchiveSet::new(9281);
    official.add_set(set.id, set.metadata.clone()).await;
    let fixture = MutationFixture::new("download_default", official.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let before = catalogue(&state).await;
        let response = public(&app, set.id, None).await;
        assert_eq!(response.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            error(&response)["error"]["code"],
            "official_archive_unavailable"
        );
        assert!(official.set_queries().await.is_empty());
        assert_eq!(official.counters(), [0; 4]);
        unchanged(&state, &before).await;
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn official_archive_download_retained_operator_package_stays_offline_readable() {
    let official = OfficialServer::start().await;
    let set = ArchiveSet::new(9301);
    official.add_set(set.id, set.metadata.clone()).await;
    let fixture = MutationFixture::new("download_retained", official.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result=tokio::spawn(async move {
        let (user,_,_,cookie)=actor(&state,"ArchiveRetained",&["beatmaps.import"]).await;
        let mut body=format!("--mutation-fixture\r\nContent-Disposition: form-data; name=\"official_set_id\"\r\n\r\n{}\r\n--mutation-fixture\r\nContent-Disposition: form-data; name=\"file\"; filename=\"owned.osz\"\r\nContent-Type: application/octet-stream\r\n\r\n",set.id).into_bytes();
        body.extend_from_slice(&set.bytes);body.extend_from_slice(b"\r\n--mutation-fixture--\r\n");
        let imported=request(&app,"/api/v1/beatmaps/import/archive",Some(&cookie),false,"multipart/form-data; boundary=mutation-fixture",body).await;
        assert_eq!(imported.0,StatusCode::OK,"{}",imported.1);
        let before=catalogue(&state).await;let queries=official.set_queries().await.len();let file_total=file_count(&state.maps_root).await;
        assert_archive(public(&app,set.id,None).await,&set);assert_archive(internal(&app,set.id,Some(user)).await,&set);
        assert_eq!(official.set_queries().await.len(),queries,"already retained immutable archives need no upstream acquisition policy");
        assert_eq!(catalogue(&state).await,before);assert_eq!(file_count(&state.maps_root).await,file_total);
    }).await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn official_archive_download_late_authenticated_import_supersedes_provider_failure_or_assets()
{
    let official = OfficialServer::start().await;
    let provider = Provider::start().await;
    let fixture = MutationFixture::new("download_late_import", official.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        configure(&state, &provider).await;
        let (user, _, _, cookie) = actor(&state, "ArchiveLateImport", &["beatmaps.import"]).await;
        for (index, provider_succeeds) in [false, true].into_iter().enumerate() {
            let set = ArchiveSet::new(9320 + index as i64);
            official.add_set(set.id, set.metadata.clone()).await;
            if provider_succeeds {
                let provider_bytes = archive(&set.charts, Some(b"different but valid provider audio asset"));
                assert_ne!(provider_bytes, set.bytes, "the matching original chart manifest does not authenticate ancillary asset identity");
                provider.reply(set.id, ProviderReply::Archive(provider_bytes)).await;
            } else {
                provider.reply(set.id, ProviderReply::Unavailable).await;
            }
            let pause = provider.pause_next().await;
            let pending_app = app.clone();
            let id = set.id;
            // Exercise both HTTP entry points while a real provider response waits.
            let pending = tokio::spawn(async move {
                if provider_succeeds { internal(&pending_app, id, Some(user)).await }
                else { public(&pending_app, id, None).await }
            });
            pause.entered().await;
            let before_import = catalogue(&state).await;
            let mut body = format!("--mutation-fixture\r\nContent-Disposition: form-data; name=\"official_set_id\"\r\n\r\n{id}\r\n--mutation-fixture\r\nContent-Disposition: form-data; name=\"file\"; filename=\"operator.osz\"\r\nContent-Type: application/octet-stream\r\n\r\n").into_bytes();
            body.extend_from_slice(&set.bytes);
            body.extend_from_slice(b"\r\n--mutation-fixture--\r\n");
            let published = request(
                &app, "/api/v1/beatmaps/import/archive", Some(&cookie), false,
                "multipart/form-data; boundary=mutation-fixture", body,
            ).await;
            assert_eq!(published.0, StatusCode::OK, "a real authorized import must publish while the independent provider waits: {}", published.1);
            let after_import = catalogue(&state).await;
            assert_eq!(after_import["beatmaps"].as_array().unwrap().len(), before_import["beatmaps"].as_array().unwrap().len() + 2);
            assert_eq!(after_import["audit_events"].as_array().unwrap().len(), before_import["audit_events"].as_array().unwrap().len() + 1);
            assert!(after_import["lazer_beatmap_updates"].as_array().unwrap().len() > before_import["lazer_beatmap_updates"].as_array().unwrap().len(), "only the real archive import publishes the native catalogue cursor");
            let retained = after_import["official_beatmapset_archives"].as_array().unwrap().iter().find(|row| row["official_set_id"] == id).unwrap();
            assert_eq!(retained["uploaded_by"], user, "attribution belongs to the authenticated operator, never the transient provider");
            assert_eq!(retained["sha256"], format!("{:x}", Sha256::digest(&set.bytes)));
            let files_after_import = file_snapshot(&state.maps_root).await;
            pause.release();
            assert_archive(pending.await.unwrap(), &set);
            assert_archive(public(&app, id, None).await, &set);
            assert_eq!(catalogue(&state).await, after_import, "returning the late retained package cannot create an additional map, audit or native cursor event");
            assert_eq!(file_snapshot(&state.maps_root).await, files_after_import, "the transient response cannot overwrite the operator's exact retained assets");
        }
        assert_eq!(provider.count(), 2, "only the two deliberately paused acquisitions reach the provider");
    }).await;
    fixture.finish(result).await;
}
