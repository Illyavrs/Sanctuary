//! Search regressions use owned PG17 schemas and a bounded loopback upstream.
use crate::{
    AppState, Router,
    maps::map_mutation_tests::{MutationFixture, actor, file_count, request},
};
use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{RawQuery, State},
    http::{HeaderMap, Request, StatusCode, header},
    routing::{get, post},
};
use illya_domain::official_set::OfficialSetMetadata;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;
use tower::ServiceExt;

const PUBLIC: &str = "/api/v1/official/beatmapsets/search";
const STAFF: &str = "/api/v1/beatmaps/official/search";
const NATIVE: &str = "/api/v1/internal/beatmapsets/search";

#[derive(Clone, Default)]
struct UpstreamState {
    pages: Arc<Mutex<HashMap<String, Value>>>,
    queries: Arc<Mutex<Vec<String>>>,
}

struct Upstream {
    origin: String,
    state: UpstreamState,
    task: tokio::task::JoinHandle<()>,
}

impl Upstream {
    async fn new(pages: &[(&str, Value)]) -> Self {
        let state = UpstreamState::default();
        state.pages.lock().await.extend(
            pages
                .iter()
                .map(|(cursor, page)| ((*cursor).to_owned(), page.clone())),
        );
        let app = Router::new()
            .route(
                "/oauth/token",
                post(|Json(body): Json<Value>| async move {
                    assert_eq!(body["grant_type"], "client_credentials");
                    assert_eq!(body["scope"], "public");
                    Json(json!({"access_token":"federated-fixture-token","expires_in":3600}))
                }),
            )
            .route("/api/v2/beatmapsets/search", get(upstream_search))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            origin,
            state,
            task,
        }
    }

    async fn calls(&self) -> usize {
        self.state.queries.lock().await.len()
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn upstream_search(
    State(state): State<UpstreamState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Json<Value> {
    assert_eq!(
        headers[header::AUTHORIZATION],
        "Bearer federated-fixture-token"
    );
    let raw = raw.unwrap_or_default();
    let url = reqwest::Url::parse(&format!("http://fixture.invalid/?{raw}")).unwrap();
    let fields = url.query_pairs().collect::<HashMap<_, _>>();
    let cursor = fields
        .get("cursor_string")
        .map_or("", |value| value.as_ref());
    let page = state
        .pages
        .lock()
        .await
        .get(cursor)
        .unwrap_or_else(|| panic!("unexpected upstream cursor {cursor}"))
        .clone();
    state.queries.lock().await.push(raw);
    Json(page)
}

fn difficulty(set: i64, id: i64, mode: i16, status: &str) -> Value {
    let rank = match status {
        "graveyard" => -2,
        "wip" => -1,
        "ranked" => 1,
        "approved" => 2,
        "qualified" => 3,
        "loved" => 4,
        _ => 0,
    };
    json!({
        "id":id,"beatmapset_id":set,"checksum":format!("{id:032x}"),
        "version":format!("Difficulty {id}"),"mode_int":mode,
        "mode":(["osu","taiko","fruits","mania"][mode as usize]),
        "status":status,"ranked":rank,"difficulty_rating":2.5,
        "total_length":120,"hit_length":110,"bpm":180.0,"ar":8.0,"cs":4.0,
        "accuracy":7.0,"drain":6.0,"max_combo":432,
        "count_circles":100,"count_sliders":140,"count_spinners":2,
        "last_updated":"2026-01-01T00:00:00Z","convert":false
    })
}

fn metadata(set: i64, status: &str, maps: Vec<Value>) -> Value {
    json!({
        "id":set,"title":format!("Federated Fixture {set:04}"),"title_unicode":"",
        "artist":"Fixture Artist","artist_unicode":"","creator":"Fixture Creator",
        "status":status,"bpm":180.0,"source":"","tags":"federated fixture",
        "nsfw":false,"submitted_date":"2026-01-01T00:00:00Z",
        "last_updated":"2026-01-01T00:00:00Z","ranked_date":null,
        "genre":{"id":1,"name":"Unspecified"},
        "language":{"id":1,"name":"Unspecified"},
        "download_disabled":false,"beatmaps":maps
    })
}

fn single(set: i64, status: &str) -> Value {
    metadata(set, status, vec![difficulty(set, set * 10, 0, status)])
}

fn upstream_page(sets: Vec<Value>, next: Option<&str>, total: usize) -> Value {
    json!({"beatmapsets":sets,"total":total,"cursor_string":next})
}

async fn publish(state: &AppState, metadata: &Value) {
    let set = metadata["id"].as_i64().unwrap();
    OfficialSetMetadata::from_upstream(metadata, set).expect("validated complete fixture metadata");
    sqlx::query("INSERT INTO beatmap_catalogue_metadata(native_set_id,metadata,verified_at) VALUES($1,$2,'2026-01-01T00:00:00Z'::timestamptz)")
        .bind(set).bind(metadata).execute(&state.db).await.unwrap();
}

async fn override_status(state: &AppState, user: i64, set: i64, id: i64, status: &str) {
    sqlx::query("INSERT INTO beatmap_status_overrides(native_id,native_set_id,status,actor_id,reason) VALUES($1,$2,$3,$4,'Owned federated fixture decision')")
        .bind(id).bind(set).bind(status).bind(user).execute(&state.db).await.unwrap();
}

fn query(fields: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("http://fixture.invalid/").unwrap();
    url.query_pairs_mut().extend_pairs(fields.iter().copied());
    url.query().unwrap_or_default().to_owned()
}

async fn website(
    app: &Router,
    endpoint: &str,
    cookie: Option<&str>,
    fields: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().uri(format!("{endpoint}?{}", query(fields)));
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn native(app: &Router, user: i64, fields: &[(&str, &str)]) -> (StatusCode, Value) {
    request(
        app,
        NATIVE,
        None,
        true,
        "application/json",
        json!({"user_id":user,"query":query(fields)})
            .to_string()
            .into_bytes(),
    )
    .await
}

fn ids(value: &Value, native: bool) -> Vec<i64> {
    let field = if native { "beatmapsets" } else { "items" };
    let id = if native { "id" } else { "official_set_id" };
    value[field]
        .as_array()
        .unwrap()
        .iter()
        .map(|set| set[id].as_i64().unwrap())
        .collect()
}

fn success((status, value): (StatusCode, Value)) -> Value {
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}

fn continuation(value: &Value, native: bool) -> String {
    let cursor = value["cursor_string"].as_str().unwrap();
    assert!(cursor.starts_with("illyaf2"), "opaque federated cursor");
    if native {
        assert_eq!(value["cursor"]["illya"], cursor);
    }
    cursor.to_owned()
}

async fn authority_snapshot(state: &AppState) -> Value {
    let mut result = serde_json::Map::new();
    for (table, order) in [
        ("beatmap_catalogue_metadata", "native_set_id"),
        ("server_catalogue_revision", "to_jsonb(r)::text"),
        ("beatmap_status_overrides", "native_id"),
        ("beatmaps", "id"),
        ("scores", "id"),
        ("audit_events", "id"),
        ("lazer_beatmap_updates", "id"),
    ] {
        let value: Value = sqlx::query_scalar(&format!(
            "SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY {order}),'[]'::jsonb) FROM {table} r"
        ))
        .fetch_one(&state.db)
        .await
        .unwrap();
        result.insert(table.into(), value);
    }
    Value::Object(result)
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn federated_search_finds_private_ranked_maps_before_empty_upstream() {
    let upstream = Upstream::new(&[("", upstream_page(vec![], None, 0))]).await;
    let fixture = MutationFixture::new("federated_promoted", upstream.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (user, _, _, cookie) = actor(&state, "FederatedPromotion", &["beatmaps.import"]).await;
        publish(&state, &single(100, "graveyard")).await;
        override_status(&state, user, 100, 1000, "ranked").await;
        let before = authority_snapshot(&state).await;
        for endpoint in [PUBLIC, STAFF] {
            let calls_before = upstream.calls().await;
            let page = success(
                website(
                    &app,
                    endpoint,
                    (endpoint == STAFF).then_some(cookie.as_str()),
                    &[("status", "ranked"), ("sort", "title_asc")],
                )
                .await,
            );
            assert_eq!(ids(&page, false), [100]);
            assert_eq!(page["items"][0]["difficulties"][0]["status"], "ranked");
            assert_eq!(
                page["items"][0]["difficulties"][0]["official_status"],
                "graveyard"
            );
            assert_eq!(page["ordering"], "server_modified_first");
            assert_eq!(page["total"], 1);
            assert_eq!(page["total_is_exact"], false);
            assert_eq!(
                upstream.calls().await,
                calls_before,
                "finite local phase avoids upstream"
            );
            let cursor = continuation(&page, false);
            let end = success(
                website(
                    &app,
                    endpoint,
                    (endpoint == STAFF).then_some(cookie.as_str()),
                    &[
                        ("status", "ranked"),
                        ("sort", "title_asc"),
                        ("cursor_string", &cursor),
                    ],
                )
                .await,
            );
            assert!(ids(&end, false).is_empty());
            assert!(end["cursor_string"].is_null());
        }
        let page = success(native(&app, user, &[("s", "ranked"), ("sort", "title_asc")]).await);
        assert_eq!(ids(&page, true), [100]);
        assert_eq!(page["beatmapsets"][0]["beatmaps"][0]["ranked"], 1);
        assert_eq!(page["total_is_exact"], false);
        let cursor = continuation(&page, true);
        let end = success(
            native(
                &app,
                user,
                &[
                    ("s", "ranked"),
                    ("sort", "title_asc"),
                    ("cursor[illya]", &cursor),
                ],
            )
            .await,
        );
        assert!(ids(&end, true).is_empty());
        assert!(end["cursor_string"].is_null());
        assert_eq!(
            upstream.calls().await,
            1,
            "all surfaces share the same bounded upstream cache"
        );
        assert_eq!(authority_snapshot(&state).await, before);
        assert_eq!(file_count(&state.maps_root).await, 0);
        assert_eq!(file_count(&state.replay_root).await, 0);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn federated_search_pages_51_private_sets_then_deduplicates_bounded_upstream_pages() {
    let upstream = Upstream::new(&[
        (
            "",
            upstream_page(vec![single(100, "ranked")], Some("upstream-page-2"), 4),
        ),
        (
            "upstream-page-2",
            upstream_page(
                vec![
                    single(101, "ranked"),
                    single(900, "ranked"),
                    single(901, "ranked"),
                ],
                None,
                4,
            ),
        ),
    ])
    .await;
    let fixture = MutationFixture::new("federated_pages", upstream.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (user, _, _, _) = actor(&state, "FederatedPagination", &[]).await;
        for set in 100..151 {
            publish(&state, &single(set, "graveyard")).await;
            override_status(&state, user, set, set * 10, "ranked").await;
        }
        let before = authority_snapshot(&state).await;
        for native_lane in [false, true] {
            let mut cursor = None::<String>;
            let mut all = Vec::new();
            for (index, expected) in [
                (100..150).collect::<Vec<_>>(),
                vec![150],
                vec![],
                vec![900, 901],
            ]
            .into_iter()
            .enumerate()
            {
                let mut fields = vec![
                    (if native_lane { "s" } else { "status" }, "ranked"),
                    ("sort", "title_asc"),
                ];
                if let Some(cursor) = cursor.as_deref() {
                    fields.push((
                        if native_lane {
                            "cursor[illya]"
                        } else {
                            "cursor_string"
                        },
                        cursor,
                    ));
                }
                let calls_before = upstream.calls().await;
                let page = success(if native_lane {
                    native(&app, user, &fields).await
                } else {
                    website(&app, PUBLIC, None, &fields).await
                });
                let calls_after = upstream.calls().await;
                assert!(
                    calls_after - calls_before <= 1,
                    "at most one upstream page per request"
                );
                if index < 2 {
                    assert_eq!(
                        calls_after, calls_before,
                        "local pagination must not query upstream"
                    );
                    assert_eq!(
                        page["total"], 51,
                        "finite phase reports its known lower bound"
                    );
                }
                assert_eq!(ids(&page, native_lane), expected);
                assert_eq!(page["ordering"], "server_modified_first");
                assert_eq!(page["total_is_exact"], false);
                all.extend(ids(&page, native_lane));
                cursor = if index == 3 {
                    assert!(page["cursor_string"].is_null());
                    None
                } else {
                    Some(continuation(&page, native_lane))
                };
            }
            assert_eq!(all, (100..151).chain([900, 901]).collect::<Vec<_>>());
        }
        assert_eq!(
            upstream.calls().await,
            2,
            "native/public continuation pages coalesce by upstream key"
        );
        for raw in upstream.state.queries.lock().await.iter() {
            assert!(
                !raw.contains("illyaf2"),
                "private cursors are not forwarded upstream"
            );
        }
        assert_eq!(authority_snapshot(&state).await, before);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn federated_search_matches_effective_mode_status_and_only_actual_modifications() {
    let upstream =
        Upstream::new(&[("", upstream_page(vec![single(101, "ranked")], None, 1))]).await;
    let fixture = MutationFixture::new("federated_filters", upstream.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (user, _, _, _) = actor(&state, "FederatedFilters", &[]).await;
        publish(
            &state,
            &metadata(
                100,
                "graveyard",
                vec![
                    difficulty(100, 1000, 0, "graveyard"),
                    difficulty(100, 1001, 3, "graveyard"),
                ],
            ),
        )
        .await;
        override_status(&state, user, 100, 1000, "ranked").await;
        publish(&state, &single(101, "ranked")).await;
        override_status(&state, user, 101, 1010, "graveyard").await;
        publish(&state, &single(102, "ranked")).await;
        override_status(&state, user, 102, 1020, "ranked").await;
        publish(
            &state,
            &metadata(
                103,
                "graveyard",
                vec![
                    difficulty(103, 1030, 0, "graveyard"),
                    difficulty(103, 1031, 3, "graveyard"),
                ],
            ),
        )
        .await;
        override_status(&state, user, 103, 1031, "ranked").await;
        let before = authority_snapshot(&state).await;
        for native_lane in [false, true] {
            for (mode, expected) in [("0", vec![100]), ("3", vec![103])] {
                let fields = [
                    (if native_lane { "s" } else { "status" }, "ranked"),
                    (if native_lane { "m" } else { "mode" }, mode),
                    ("status_modified_only", "true"),
                ];
                let page = success(if native_lane {
                    native(&app, user, &fields).await
                } else {
                    website(&app, PUBLIC, None, &fields).await
                });
                assert_eq!(
                    ids(&page, native_lane),
                    expected,
                    "one difficulty must satisfy both mode and effective status"
                );
                assert_eq!(page["total"], 1);
                assert_eq!(page["total_is_exact"], true);
                assert!(page["cursor_string"].is_null());
            }
            let fields = [
                (if native_lane { "s" } else { "status" }, "any"),
                ("sort", "title_asc"),
                ("status_modified_only", "true"),
            ];
            let page = success(if native_lane {
                native(&app, user, &fields).await
            } else {
                website(&app, PUBLIC, None, &fields).await
            });
            assert_eq!(
                ids(&page, native_lane),
                [100, 101, 103],
                "same-status authority rows are not effective modifications"
            );
            assert_eq!(page["total"], 3);
            assert_eq!(page["total_is_exact"], true);
        }
        assert_eq!(
            upstream.calls().await,
            0,
            "modified-only never requests an official page"
        );
        // An officially Ranked map demoted here must also disappear from the ordinary Ranked union.
        let first = success(
            website(
                &app,
                PUBLIC,
                None,
                &[("status", "ranked"), ("sort", "title_asc")],
            )
            .await,
        );
        assert_eq!(ids(&first, false), [100, 103]);
        let cursor = continuation(&first, false);
        let upstream_result = success(
            website(
                &app,
                PUBLIC,
                None,
                &[
                    ("status", "ranked"),
                    ("sort", "title_asc"),
                    ("cursor_string", &cursor),
                ],
            )
            .await,
        );
        assert!(
            ids(&upstream_result, false).is_empty(),
            "demoted official difficulty must not pass the final effective filter"
        );
        let first = success(native(&app, user, &[("s", "ranked"), ("sort", "title_asc")]).await);
        assert_eq!(ids(&first, true), [100, 103]);
        let cursor = continuation(&first, true);
        let upstream_result = success(
            native(
                &app,
                user,
                &[
                    ("s", "ranked"),
                    ("sort", "title_asc"),
                    ("cursor[illya]", &cursor),
                ],
            )
            .await,
        );
        assert!(
            ids(&upstream_result, true).is_empty(),
            "native Ranked browsing must also exclude the local demotion"
        );
        assert_eq!(upstream.calls().await, 1);
        assert_eq!(authority_snapshot(&state).await, before);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn federated_search_cursors_bind_query_and_revision_without_upstream_work() {
    let upstream = Upstream::new(&[("", upstream_page(vec![], None, 0))]).await;
    let fixture = MutationFixture::new("federated_cursor", upstream.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (user, _, _, _) = actor(&state, "FederatedCursor", &[]).await;
        publish(&state, &single(100, "graveyard")).await;
        override_status(&state, user, 100, 1000, "ranked").await;
        let public_page = success(website(&app, PUBLIC, None, &[("status", "ranked")]).await);
        let public_cursor = continuation(&public_page, false);
        let native_page = success(native(&app, user, &[("s", "ranked")]).await);
        let native_cursor = continuation(&native_page, true);
        for fields in [
            vec![
                ("status", "ranked"),
                ("q", "different"),
                ("cursor_string", public_cursor.as_str()),
            ],
            vec![
                ("status", "loved"),
                ("cursor_string", public_cursor.as_str()),
            ],
            vec![
                ("status", "ranked"),
                ("mode", "3"),
                ("cursor_string", public_cursor.as_str()),
            ],
        ] {
            assert_eq!(
                website(&app, PUBLIC, None, &fields).await.0,
                StatusCode::CONFLICT
            );
        }
        assert_eq!(
            native(
                &app,
                user,
                &[
                    ("s", "ranked"),
                    ("q", "different"),
                    ("cursor[illya]", &native_cursor)
                ]
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        for fields in [
            vec![("status", "ranked"), ("cursor_string", "illyaf2-not-hex")],
            vec![("status", "ranked"), ("cursor_string", "illyaf2:00")],
        ] {
            assert_eq!(
                website(&app, PUBLIC, None, &fields).await.0,
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            native(
                &app,
                user,
                &[("s", "ranked"), ("cursor[illya]", "illyaf2-not-hex")]
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        sqlx::query("UPDATE beatmap_status_overrides SET status='pending' WHERE native_id=1000")
            .execute(&state.db)
            .await
            .unwrap();
        let after_change = authority_snapshot(&state).await;
        assert_eq!(
            website(
                &app,
                PUBLIC,
                None,
                &[("status", "ranked"), ("cursor_string", &public_cursor)]
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            native(
                &app,
                user,
                &[("s", "ranked"), ("cursor[illya]", &native_cursor)]
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            upstream.calls().await,
            0,
            "invalid continuations must fail before official work"
        );
        assert_eq!(authority_snapshot(&state).await, after_change);
    })
    .await;
    fixture.finish(result).await;
}
