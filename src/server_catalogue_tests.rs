//! Effective discovery fixtures use owned PG17 schemas and never contact upstream.
use crate::native_map_tests::OfficialServer;
use crate::{
    AppState, Router,
    maps::map_mutation_tests::{MutationFixture, actor, file_count, request},
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use illya_domain::official_set::OfficialSetMetadata;
use serde_json::{Value, json};
use tower::ServiceExt;

const PUBLIC: &str = "/api/v1/server/beatmapsets";
const NATIVE: &str = "/api/v1/internal/beatmapsets/catalogue-search";

fn difficulty(set: i64, id: i64, version: &str, mode: i16, status: &str) -> Value {
    let ranked = match status {
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
        "version":version,"mode_int":mode,"mode":(["osu","taiko","fruits","mania"][mode as usize]),
        "status":status,"ranked":ranked,"difficulty_rating":2.5,
        "total_length":120,"hit_length":110,"bpm":180.0,"ar":8.0,"cs":4.0,
        "accuracy":7.0,"drain":6.0,"max_combo":432,
        "count_circles":100,"count_sliders":140,"count_spinners":2,
        "last_updated":"2026-01-01T00:00:00Z","convert":false
    })
}

fn metadata(set: i64, status: &str, maps: Vec<Value>) -> Value {
    json!({
        "id":set,"title":"Catalogue Fixture","title_unicode":"",
        "artist":"Fixture Artist","artist_unicode":"","creator":"Fixture Creator",
        "status":status,"bpm":180.0,"source":"","tags":"catalogue fixture",
        "nsfw":false,"submitted_date":"2026-01-01T00:00:00Z",
        "last_updated":"2026-01-01T00:00:00Z","ranked_date":null,
        "genre":{"id":1,"name":"Unspecified"},
        "language":{"id":1,"name":"Unspecified"},
        "download_disabled":false,"beatmaps":maps
    })
}

async fn publish_metadata(state: &AppState, value: &Value) {
    let set = value["id"].as_i64().unwrap();
    OfficialSetMetadata::from_upstream(value, set).expect("valid original native metadata fixture");
    sqlx::query("INSERT INTO beatmap_catalogue_metadata(native_set_id,metadata,verified_at) VALUES($1,$2,'2026-01-01T00:00:00Z'::timestamptz)")
        .bind(set).bind(value).execute(&state.db).await.unwrap();
}

async fn override_status(state: &AppState, actor: i64, set: i64, id: i64, status: &str) {
    sqlx::query("INSERT INTO beatmap_status_overrides(native_id,native_set_id,status,actor_id,reason) VALUES($1,$2,$3,$4,'Owned effective catalogue fixture')")
        .bind(id).bind(set).bind(status).bind(actor).execute(&state.db).await.unwrap();
}

async fn store_official(state: &AppState, set: i64, map: &Value, checksum: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO beatmaps(official_id,official_set_id,title,artist,creator,version,mode,status,source,stars,length_seconds,bpm,max_combo,md5,osu_path,created_at) VALUES($1,$2,'Retained content title','Fixture Artist','Fixture Creator',$3,$4,'graveyard','official',$5,$6,180,432,$7,'private-catalogue-fixture.osu','2026-01-01T00:00:00Z'::timestamptz) RETURNING id")
        .bind(map["id"].as_i64().unwrap()).bind(set)
        .bind(map["version"].as_str().unwrap()).bind(map["mode_int"].as_i64().unwrap() as i16)
        .bind(map["difficulty_rating"].as_f64().unwrap()).bind(map["total_length"].as_i64().unwrap() as i32)
        .bind(checksum).fetch_one(&state.db).await.unwrap()
}

async fn store_local(state: &AppState, owner: i64, index: i64) -> i64 {
    sqlx::query_scalar("INSERT INTO beatmaps(uploaded_by,title,artist,creator,version,mode,status,source,stars,length_seconds,bpm,max_combo,md5,osu_path,created_at) VALUES($1,'Catalogue Fixture','Fixture Artist','Fixture Creator','Uploaded',0,'ranked','local',2.5,120,180,432,$2,'private-local-catalogue-fixture.osu','2026-01-01T00:00:00Z'::timestamptz) RETURNING native_set_id")
        .bind(owner).bind(format!("{index:032x}")).fetch_one(&state.db).await.unwrap()
}

fn path(parameters: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("http://fixture.invalid").unwrap();
    url.query_pairs_mut()
        .extend_pairs(parameters.iter().copied());
    format!("{PUBLIC}?{}", url.query().unwrap_or_default())
}

async fn get(app: &Router, parameters: &[(&str, &str)]) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(path(parameters))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn catalogue(app: &Router, parameters: &[(&str, &str)]) -> Value {
    let (status, value) = get(app, parameters).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}

async fn native(app: &Router, user: i64, query: &str) -> (StatusCode, Value) {
    request(
        app,
        NATIVE,
        None,
        true,
        "application/json",
        json!({"user_id":user,"query":query})
            .to_string()
            .into_bytes(),
    )
    .await
}

async fn native_page(app: &Router, user: i64, query: &str) -> Value {
    let (status, value) = native(app, user, query).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}

async fn refresh(app: &Router, cookie: &str, set: i64, value: Value) -> (StatusCode, Value) {
    request(
        app,
        &format!("/api/v1/admin/beatmapsets/{set}/catalogue-metadata"),
        Some(cookie),
        false,
        "application/json",
        value.to_string().into_bytes(),
    )
    .await
}

fn public_ids(page: &Value) -> Vec<i64> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| match item["kind"].as_str().unwrap() {
            "official_metadata" => item["official"]["official_set_id"].as_i64().unwrap(),
            "stored" => item["stored"]["native_set_id"].as_i64().unwrap(),
            kind => panic!("unexpected catalogue item {kind}"),
        })
        .collect()
}

fn native_ids(page: &Value) -> Vec<i64> {
    page["beatmapsets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_i64().unwrap())
        .collect()
}

fn assert_page(page: &Value, ids: &[i64], total: i64, number: i64, size: i64) {
    assert_eq!(public_ids(page), ids);
    assert_eq!(page["total"], total);
    assert_eq!(page["page"], number);
    assert_eq!(page["page_size"], size);
}

fn original(official: &Value, id: i64) -> &Value {
    official["difficulties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|map| map["official_id"] == id)
        .unwrap()
}

fn assert_public(value: &Value) {
    let encoded = value.to_string();
    for private in [
        "private-catalogue-fixture",
        "private-local-catalogue-fixture",
        "Owned effective catalogue fixture",
        "actor_id",
        "osu_path",
        "archive_path",
        "sha256",
        "member_md5",
    ] {
        assert!(
            !encoded.contains(private),
            "private catalogue data leaked: {private}"
        );
    }
}

async fn snapshot(state: &AppState) -> Value {
    let mut result = serde_json::Map::new();
    for (table, order) in [
        ("beatmap_catalogue_metadata", "native_set_id"),
        ("server_catalogue_revision", "to_jsonb(r)::text"),
        ("beatmap_status_overrides", "native_id"),
        ("beatmaps", "id"),
        ("beatmap_packages", "id"),
        ("beatmapset_availability", "native_set_id"),
        ("official_beatmapset_archives", "official_set_id"),
        ("official_beatmapset_archive_members", "beatmap_id"),
        ("beatmap_filename_aliases", "filename,beatmap_id"),
        ("scores", "id"),
        ("audit_events", "id"),
        ("lazer_beatmap_updates", "id"),
    ] {
        let rows: Value = sqlx::query_scalar(&format!(
            "SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY {order}),'[]'::jsonb) FROM {table} r"
        ))
        .fetch_one(&state.db)
        .await
        .unwrap();
        result.insert(table.into(), rows);
    }
    Value::Object(result)
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn server_catalogue_discovers_metadata_only_ranked_difficulties_and_keeps_original_siblings()
{
    let fixture = MutationFixture::new("effective_metadata", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (user, _, _, _) = actor(&state, "EffectiveMetadata", &[]).await;
        for (set, status) in [(100, "graveyard"), (200, "pending")] {
            let maps = vec![
                difficulty(set, set * 10, "Ranked Original", 0, status),
                difficulty(set, set * 10 + 1, "Unselected Original", 3, "graveyard"),
            ];
            publish_metadata(&state, &metadata(set, status, maps)).await;
            override_status(&state, user, set, set * 10, "ranked").await;
        }
        // A verified snapshot without a server decision is not an added catalogue member.
        publish_metadata(
            &state,
            &metadata(
                300,
                "ranked",
                vec![difficulty(300, 3000, "Official Only", 0, "ranked")],
            ),
        )
        .await;
        let before = snapshot(&state).await;
        let page = catalogue(&app, &[("status", "ranked"), ("sort", "title")]).await;
        assert_page(&page, &[100, 200], 2, 1, 24);
        for (index, upstream_status) in ["graveyard", "pending"].iter().enumerate() {
            let item = &page["items"][index];
            let official = &item["official"];
            let set = official["official_set_id"].as_i64().unwrap();
            assert_eq!(item["kind"], "official_metadata");
            assert_eq!(official["official_status"], *upstream_status);
            assert_eq!(official["metadata_only"], true);
            assert_eq!(official["local"]["stored_count"], 0);
            assert_eq!(official["local"]["archive_available"], false);
            assert_eq!(original(official, set * 10)["status"], "ranked");
            assert_eq!(
                original(official, set * 10)["official_status"],
                *upstream_status
            );
            assert_eq!(original(official, set * 10 + 1)["status"], "graveyard");
            assert!(
                official["difficulties"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|map| map.get("local_id").is_none() && map["archive_available"] == false)
            );
            assert_public(item);
        }
        let native = native_page(&app, user, "s=ranked&sort=title_asc").await;
        assert_eq!(native_ids(&native), [100, 200]);
        assert_eq!(native["total"], 2);
        for set in native["beatmapsets"].as_array().unwrap() {
            assert_eq!(set["availability"]["download_disabled"], true);
            assert_eq!(set["beatmaps"][0]["status"], "ranked");
            assert_eq!(set["beatmaps"][0]["ranked"], 1);
            assert_eq!(set["beatmaps"][1]["status"], "graveyard");
            assert_eq!(set["beatmaps"][1]["ranked"], -2);
        }
        assert_eq!(
            snapshot(&state).await,
            before,
            "discovery must not publish maps, scores, files or authority writes"
        );
        assert_eq!(file_count(&state.maps_root).await, 0);
        assert_eq!(file_count(&state.replay_root).await, 0);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn server_catalogue_combined_filters_match_one_original_difficulty() {
    let fixture = MutationFixture::new("effective_filters", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (user, _, _, _) = actor(&state, "EffectiveFilters", &[]).await;
        publish_metadata(
            &state,
            &metadata(
                100,
                "graveyard",
                vec![
                    difficulty(100, 1000, "Ranked Target", 0, "graveyard"),
                    difficulty(100, 1001, "Sibling Needle", 3, "graveyard"),
                ],
            ),
        )
        .await;
        override_status(&state, user, 100, 1000, "ranked").await;
        let before = snapshot(&state).await;
        for parameters in [
            vec![("mode", "3"), ("status", "ranked")],
            vec![("q", "Sibling Needle"), ("status", "ranked")],
            vec![("q", "Sibling Needle"), ("mode", "0")],
        ] {
            assert_page(&catalogue(&app, &parameters).await, &[], 0, 1, 24);
        }
        assert_page(
            &catalogue(
                &app,
                &[("q", "Ranked Target"), ("mode", "0"), ("status", "ranked")],
            )
            .await,
            &[100],
            1,
            1,
            24,
        );
        assert_page(
            &catalogue(
                &app,
                &[
                    ("q", "Sibling Needle"),
                    ("mode", "3"),
                    ("status", "graveyard"),
                ],
            )
            .await,
            &[100],
            1,
            1,
            24,
        );
        for query in [
            "s=ranked&m=3",
            "s=ranked&q=Sibling%20Needle",
            "m=0&q=Sibling%20Needle",
        ] {
            let page = native_page(&app, user, query).await;
            assert!(native_ids(&page).is_empty());
            assert_eq!(page["total"], 0);
        }
        assert_eq!(
            native_ids(&native_page(&app, user, "s=ranked&m=0&q=Ranked%20Target").await),
            [100]
        );
        assert_eq!(snapshot(&state).await, before);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn server_catalogue_status_reversal_and_withdrawal_change_membership_and_total_immediately() {
    let fixture = MutationFixture::new("effective_reversal", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (user, _, _, _) = actor(&state, "EffectiveReversal", &[]).await;
        for set in [100, 200] {
            publish_metadata(&state, &metadata(set, "graveyard", vec![difficulty(set, set * 10, "Original", 0, "graveyard")])).await;
            override_status(&state, user, set, set * 10, "ranked").await;
        }
        assert_page(&catalogue(&app, &[("status", "ranked"), ("sort", "title")]).await, &[100, 200], 2, 1, 24);
        sqlx::query("UPDATE beatmap_status_overrides SET status='graveyard' WHERE native_id=1000").execute(&state.db).await.unwrap();
        assert_page(&catalogue(&app, &[("status", "ranked")]).await, &[200], 1, 1, 24);
        assert_eq!(native_ids(&native_page(&app, user, "s=ranked").await), [200]);
        sqlx::query("INSERT INTO beatmapset_availability(native_set_id,withdrawn,revision,reason,actor_id) VALUES(200,true,1,'Private effective catalogue withdrawal',$1)").bind(user).execute(&state.db).await.unwrap();
        let before = snapshot(&state).await;
        assert_page(&catalogue(&app, &[("status", "ranked"), ("page", "2"), ("page_size", "1")]).await, &[], 0, 2, 1);
        assert_page(&catalogue(&app, &[("sort", "title")]).await, &[100], 1, 1, 24);
        let page = native_page(&app, user, "s=ranked").await;
        assert!(native_ids(&page).is_empty());
        assert_eq!(page["total"], 0);
        assert_eq!(snapshot(&state).await, before);
    }).await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn server_catalogue_partial_import_keeps_one_set_and_links_only_matching_content() {
    let fixture = MutationFixture::new("effective_partial", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (user, _, _, _) = actor(&state, "EffectivePartial", &[]).await;
        let first = difficulty(100, 1000, "Ranked Original", 0, "graveyard");
        let second = difficulty(100, 1001, "Unstored Original", 3, "graveyard");
        publish_metadata(
            &state,
            &metadata(100, "graveyard", vec![first.clone(), second]),
        )
        .await;
        override_status(&state, user, 100, 1000, "ranked").await;
        let local = store_official(&state, 100, &first, first["checksum"].as_str().unwrap()).await;
        let before = snapshot(&state).await;
        let page = catalogue(&app, &[("status", "ranked")]).await;
        assert_page(&page, &[100], 1, 1, 24);
        assert_eq!(page["items"][0]["kind"], "official_metadata");
        let official = &page["items"][0]["official"];
        assert_eq!(official["title"], "Catalogue Fixture");
        assert_eq!(official["metadata_only"], false);
        assert_eq!(official["local"]["stored_count"], 1);
        assert_eq!(official["difficulties"].as_array().unwrap().len(), 2);
        assert_eq!(original(official, 1000)["local_id"], local);
        assert!(original(official, 1001).get("local_id").is_none());
        assert!(
            official["difficulties"]
                .as_array()
                .unwrap()
                .iter()
                .all(|map| map["archive_available"] == false)
        );
        assert_eq!(
            native_ids(&native_page(&app, user, "s=ranked").await),
            [100]
        );
        assert_eq!(snapshot(&state).await, before);
        // Retained checksums are immutable. Simulate a conflicting persisted
        // upstream snapshot while preserving the actual retained identity.
        sqlx::query("UPDATE beatmap_catalogue_metadata SET metadata=jsonb_set(metadata,'{beatmaps,0,checksum}',to_jsonb($1::text)) WHERE native_set_id=$2")
            .bind("f".repeat(32)).bind(100_i64)
            .execute(&state.db)
            .await
            .unwrap();
        let before = snapshot(&state).await;
        let (status,page) = get(&app, &[("status", "ranked")]).await;
        assert_eq!(status,StatusCode::CONFLICT,"{page}");
        assert_eq!(page["error"]["code"],"official_version_conflict");
        assert_eq!(snapshot(&state).await, before);
        assert_eq!(file_count(&state.maps_root).await, 0);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn server_catalogue_tied_sort_has_exact_canonical_counts_on_every_page() {
    let fixture = MutationFixture::new("effective_pages", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (user, _, _, _) = actor(&state, "EffectivePages", &[]).await;
        for set in [300, 100, 200] {
            let map = difficulty(set, set * 10, "Original", 0, "graveyard");
            publish_metadata(&state, &metadata(set, "graveyard", vec![map.clone()])).await;
            override_status(&state, user, set, set * 10, "ranked").await;
            if set == 200 {
                let stored =
                    store_official(&state, set, &map, map["checksum"].as_str().unwrap()).await;
                sqlx::query("UPDATE beatmaps SET title='Catalogue Fixture' WHERE id=$1")
                    .bind(stored)
                    .execute(&state.db)
                    .await
                    .unwrap();
            }
        }
        let before = snapshot(&state).await;
        for sort in ["title", "stars", "length"] {
            for (page, expected) in [("1", vec![100, 200]), ("2", vec![300]), ("3", vec![])] {
                assert_page(
                    &catalogue(
                        &app,
                        &[
                            ("status", "ranked"),
                            ("sort", sort),
                            ("page", page),
                            ("page_size", "2"),
                        ],
                    )
                    .await,
                    &expected,
                    3,
                    page.parse().unwrap(),
                    2,
                );
            }
        }
        assert_page(
            &catalogue(
                &app,
                &[
                    ("status", "ranked"),
                    ("sort", "title"),
                    ("page", "99"),
                    ("page_size", "2"),
                ],
            )
            .await,
            &[],
            3,
            99,
            2,
        );
        assert_eq!(snapshot(&state).await, before);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn server_catalogue_upload_filters_exclude_metadata_only_members() {
    let fixture = MutationFixture::new("effective_uploads", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (owner, _, _, _) = actor(&state, "EffectiveUploadOwner", &[]).await;
        let (other, _, _, _) = actor(&state, "EffectiveUploadOther", &[]).await;
        publish_metadata(
            &state,
            &metadata(
                100,
                "pending",
                vec![difficulty(100, 1000, "Original", 0, "pending")],
            ),
        )
        .await;
        override_status(&state, owner, 100, 1000, "ranked").await;
        let owned_set = store_local(&state, owner, 9001).await;
        store_local(&state, other, 9002).await;
        let before = snapshot(&state).await;
        let owner_id = owner.to_string();
        assert_page(
            &catalogue(&app, &[("uploaded_by", &owner_id)]).await,
            &[owned_set],
            1,
            1,
            24,
        );
        assert_page(
            &catalogue(
                &app,
                &[("uploaded_only", "true"), ("uploaded_by", &owner_id)],
            )
            .await,
            &[owned_set],
            1,
            1,
            24,
        );
        assert_page(
            &catalogue(&app, &[("uploaded_only", "true"), ("source", "official")]).await,
            &[],
            0,
            1,
            24,
        );
        assert_page(
            &catalogue(&app, &[("source", "official"), ("status", "ranked")]).await,
            &[100],
            1,
            1,
            24,
        );
        assert_eq!(
            catalogue(&app, &[("uploaded_only", "true")]).await["total"],
            2
        );
        assert_eq!(catalogue(&app, &[]).await["total"], 3);
        assert_eq!(snapshot(&state).await, before);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn server_catalogue_native_guards_and_unsupported_filters_do_not_mutate_authority() {
    let fixture = MutationFixture::new("effective_guards", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (user, _, _, _) = actor(&state, "EffectiveGuards", &[]).await;
        let before = snapshot(&state).await;
        assert_eq!(
            request(
                &app,
                NATIVE,
                None,
                false,
                "application/json",
                b"not JSON".to_vec()
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(native(&app, 0, "s=ranked").await.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            native(&app, i64::MAX, "s=ranked").await.0,
            StatusCode::UNAUTHORIZED
        );
        for query in [
            "g=1",
            "l=1",
            "c=converts",
            "e=video",
            "sort=rating_desc",
            "sort=plays_desc",
            "sort=favourites_desc",
            "sort=nominations_desc",
            "played=played",
            "r=SH",
            "s=mine",
            "s=favourites",
        ] {
            let (status, value) = native(&app, user, query).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {value}");
            assert_eq!(
                value["error"]["code"], "unsupported_server_search_filter",
                "{query}"
            );
        }
        for sort in [
            "ranked_asc",
            "ranked_desc",
            "updated_asc",
            "updated_desc",
            "title_asc",
            "title_desc",
            "artist_asc",
            "artist_desc",
            "difficulty_asc",
            "difficulty_desc",
            "relevance_asc",
            "relevance_desc",
        ] {
            let query = format!("s=any&sort={sort}&g=0&l=0&c=&e=&nsfw=true");
            let page = native_page(&app, user, &query).await;
            assert_eq!(page["total"], 0);
        }
        for query in [
            "m=4",
            "q=%ff",
            "s=ranked&s=any",
            "cursor%5Bprivate%5D=x",
            "nsfw=1",
        ] {
            assert_eq!(
                native(&app, user, query).await.0,
                StatusCode::BAD_REQUEST,
                "{query}"
            );
        }
        assert_eq!(snapshot(&state).await, before);
    })
    .await;
    fixture.finish(result).await;
}

fn cursor_query(base: &str, token: &str, structured: bool) -> String {
    let mut url = reqwest::Url::parse(&format!("http://fixture.invalid/?{base}")).unwrap();
    url.query_pairs_mut().append_pair(
        if structured {
            "cursor[illya]"
        } else {
            "cursor_string"
        },
        token,
    );
    url.query().unwrap().to_owned()
}

fn cursor(page: &Value) -> String {
    let token = page["cursor_string"]
        .as_str()
        .expect("nonterminal native page cursor");
    assert!(!token.is_empty());
    assert_eq!(page["cursor"], json!({"illya":token}));
    token.to_owned()
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn server_catalogue_native_cursor_pages_are_truthful_query_bound_and_revision_bound() {
    let fixture = MutationFixture::new("effective_cursor", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (user, _, _, _) = actor(&state, "EffectiveCursor", &[]).await;
        for set in 1000..1051 {
            publish_metadata(&state, &metadata(set, "graveyard", vec![difficulty(set, set * 10, "Original", 0, "graveyard")])).await;
            override_status(&state, user, set, set * 10, "ranked").await;
        }
        let query = "s=ranked&sort=title_asc";
        let before = snapshot(&state).await;
        let first = native_page(&app, user, query).await;
        assert_eq!(native_ids(&first), (1000..1050).collect::<Vec<_>>());
        assert_eq!(first["total"], 51);
        let token = cursor(&first);
        for structured in [false, true] {
            let second = native_page(&app, user, &cursor_query(query, &token, structured)).await;
            assert_eq!(native_ids(&second), [1050]);
            assert_eq!(second["total"], 51);
            assert_eq!(second["cursor"], Value::Null);
            assert_eq!(second["cursor_string"], Value::Null);
        }
        assert_eq!(native(&app, user, &cursor_query("s=any&sort=title_asc", &token, false)).await.0, StatusCode::CONFLICT);
        assert_eq!(snapshot(&state).await, before);

        sqlx::query("UPDATE beatmap_catalogue_metadata SET metadata=jsonb_set(metadata,'{title}','\"Changed metadata title\"'::jsonb) WHERE native_set_id=1000").execute(&state.db).await.unwrap();
        let before = snapshot(&state).await;
        assert_eq!(native(&app, user, &cursor_query(query, &token, false)).await.0, StatusCode::CONFLICT, "metadata publication invalidates a native cursor");
        assert_eq!(snapshot(&state).await, before);

        let token = cursor(&native_page(&app, user, query).await);
        sqlx::query("UPDATE beatmap_status_overrides SET status='graveyard' WHERE native_id=10000").execute(&state.db).await.unwrap();
        let before = snapshot(&state).await;
        assert_eq!(native(&app, user, &cursor_query(query, &token, true)).await.0, StatusCode::CONFLICT, "status changes invalidate a native cursor");
        assert_eq!(snapshot(&state).await, before);
        sqlx::query("UPDATE beatmap_status_overrides SET status='ranked' WHERE native_id=10000").execute(&state.db).await.unwrap();

        let token = cursor(&native_page(&app, user, query).await);
        sqlx::query("INSERT INTO beatmapset_availability(native_set_id,withdrawn,revision,reason,actor_id) VALUES(1000,true,1,'Private cursor withdrawal',$1)").bind(user).execute(&state.db).await.unwrap();
        let before = snapshot(&state).await;
        assert_eq!(native(&app, user, &cursor_query(query, &token, false)).await.0, StatusCode::CONFLICT, "withdrawal invalidates a native cursor");
        assert_eq!(snapshot(&state).await, before);
        sqlx::query("UPDATE beatmapset_availability SET withdrawn=false,revision=2 WHERE native_set_id=1000").execute(&state.db).await.unwrap();

        let token = cursor(&native_page(&app, user, query).await);
        let map = difficulty(1000, 10000, "Original", 0, "graveyard");
        store_official(&state, 1000, &map, map["checksum"].as_str().unwrap()).await;
        let before = snapshot(&state).await;
        assert_eq!(native(&app, user, &cursor_query(query, &token, true)).await.0, StatusCode::CONFLICT, "content availability invalidates a native cursor");
        assert_eq!(snapshot(&state).await, before);
        assert_eq!(native_page(&app, user, query).await["total"], 51, "partial storage must not double-count its canonical set");
        assert_eq!(file_count(&state.maps_root).await, 0);
    }).await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn server_catalogue_legacy_overrides_require_atomic_current_checksum_reaffirmation() {
    let server = OfficialServer::start().await;
    server
        .add_set(
            100,
            metadata(
                100,
                "graveyard",
                vec![
                    difficulty(100, 1000, "Easy", 0, "graveyard"),
                    difficulty(100, 1001, "Hard", 0, "graveyard"),
                ],
            ),
        )
        .await;
    let fixture = MutationFixture::new("effective_reaffirm", server.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (staff, _, _, cookie) = actor(&state, "EffectiveReaffirm", &["beatmaps.moderate"]).await;
        override_status(&state, staff, 100, 1000, "ranked").await;
        override_status(&state, staff, 100, 1001, "loved").await;
        let before = snapshot(&state).await;
        let (status, value) = refresh(&app, &cookie, 100, json!({"reason":"Refresh legacy decisions"})).await;
        assert_eq!(status, StatusCode::CONFLICT, "unproven metadata-only decisions cannot silently attach to current content: {value}");
        assert_eq!(snapshot(&state).await, before);
        let valid = json!({"reason":"Reviewed both current originals","reaffirm_difficulties":[
            {"native_id":1001,"checksum":format!("{:032x}",1001),"expected_revision":1,"expected_status_revision":null},
            {"native_id":1000,"checksum":format!("{:032x}",1000),"expected_revision":1,"expected_status_revision":null}
        ]});
        for failure in ["stale", "checksum", "subset"] {
            let mut input = valid.clone();
            match failure {
                "stale" => input["reaffirm_difficulties"][0]["expected_revision"] = json!(0),
                "checksum" => input["reaffirm_difficulties"][0]["checksum"] = json!("f".repeat(32)),
                "subset" => { input["reaffirm_difficulties"].as_array_mut().unwrap().remove(0); }
                _ => unreachable!(),
            }
            let (status, value) = refresh(&app, &cookie, 100, input).await;
            assert_eq!(status, StatusCode::CONFLICT, "{failure}: {value}");
            assert_eq!(snapshot(&state).await, before, "a later {failure} failure must roll back every tentative reaffirmation, audit and epoch change");
        }
        let (status, value) = refresh(&app, &cookie, 100, valid).await;
        assert_eq!(status, StatusCode::OK, "both current checksum decisions can repair the legacy set atomically: {value}");
        let repaired = snapshot(&state).await;
        assert_eq!(repaired["beatmap_catalogue_metadata"].as_array().unwrap().len(), 1);
        assert_eq!(repaired["beatmap_catalogue_metadata"][0]["metadata"]["beatmaps"].as_array().unwrap().len(), 2);
        assert_eq!(repaired["beatmap_status_overrides"][0]["status"], "ranked");
        assert_eq!(repaired["beatmap_status_overrides"][1]["status"], "loved");
        assert_eq!(repaired["beatmaps"], json!([]));
        let audit: Value = sqlx::query_scalar("SELECT details FROM audit_events WHERE action='beatmaps.catalogue_metadata' ORDER BY id DESC LIMIT 1")
            .fetch_one(&state.db).await.unwrap();
        assert_eq!(audit["reaffirmed_difficulty_ids"], json!([1000, 1001]), "input order cannot change the atomic canonical decision order");
        let (status, value) = refresh(&app, &cookie, 100, json!({"reason":"Refresh verified original metadata"})).await;
        assert_eq!(status, StatusCode::OK, "published checksum proof supports an ordinary refresh: {value}");
        assert_eq!(snapshot(&state).await["beatmap_status_overrides"], repaired["beatmap_status_overrides"], "ordinary refresh cannot silently reauthorize existing decisions");
        assert_page(&catalogue(&app, &[("status", "ranked")]).await, &[100], 1, 1, 24);
        assert_eq!(file_count(&state.maps_root).await, 0);
        assert_eq!(server.counters()[3], 0);
    }).await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn server_catalogue_approved_nomination_proof_repairs_only_the_same_original_checksum() {
    for changed in [false, true] {
        let server = OfficialServer::start().await;
        let mut original = difficulty(100, 1000, "Easy", 0, "graveyard");
        if changed {
            original["checksum"] = json!("f".repeat(32));
        }
        server
            .add_set(
                100,
                metadata(
                    100,
                    "graveyard",
                    vec![original, difficulty(100, 1001, "Hard", 0, "graveyard")],
                ),
            )
            .await;
        let fixture = MutationFixture::new(
            if changed {
                "effective_proof_changed"
            } else {
                "effective_proof_same"
            },
            server.origin.clone(),
        )
        .await;
        let state = fixture.state.clone();
        let app = fixture.app.clone();
        let result = tokio::spawn(async move {
            let (staff, _, _, cookie) = actor(&state, "EffectiveApprovedProof", &["beatmaps.moderate"]).await;
            override_status(&state, staff, 100, 1000, "ranked").await;
            let proof = json!({"title":"Private original nomination","artist":"Fixture Artist","creator":"Fixture Creator","difficulty_count":2,"upstream_status":"graveyard","manifest":[
                {"native_id":1000,"md5":format!("{:032x}",1000),"version":"Easy","mode":0,"stars":2.5,"status":"graveyard"},
                {"native_id":1001,"md5":format!("{:032x}",1001),"version":"Hard","mode":0,"stars":2.5,"status":"graveyard"}
            ]});
            sqlx::query("INSERT INTO beatmap_rank_requests(native_set_id,source,requester_id,reason,status,metadata,review_reason,reviewed_by,approved_difficulty_ids,reviewed_at) VALUES(100,'official',$1,'Legacy approved nomination','approved',$2,'Legacy approval',$1,ARRAY[1000]::bigint[],clock_timestamp())")
                .bind(staff).bind(proof).execute(&state.db).await.unwrap();
            let before = snapshot(&state).await;
            let (status, value) = refresh(&app, &cookie, 100, json!({"reason":"Recover approved catalogue metadata"})).await;
            if changed {
                assert_eq!(status, StatusCode::CONFLICT, "a legacy approval cannot authorize changed original bytes: {value}");
                assert_eq!(snapshot(&state).await, before);
            } else {
                assert_eq!(status, StatusCode::OK, "immutable approved checksum proof can recover missing public metadata: {value}");
                let after = snapshot(&state).await;
                assert_eq!(after["beatmap_status_overrides"], before["beatmap_status_overrides"]);
                assert_eq!(after["beatmap_catalogue_metadata"].as_array().unwrap().len(), 1);
                assert_eq!(after["beatmap_catalogue_metadata"][0]["metadata"]["beatmaps"].as_array().unwrap().len(), 2);
                assert!(!after["beatmap_catalogue_metadata"].to_string().contains("Private original nomination"));
                assert_page(&catalogue(&app, &[("status", "ranked")]).await, &[100], 1, 1, 24);
            }
            assert_eq!(file_count(&state.maps_root).await, 0);
        }).await;
        fixture.finish(result).await;
    }
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn server_catalogue_refresh_rejects_growth_beyond_an_immutable_official_archive() {
    let server = OfficialServer::start().await;
    let maps = vec![
        difficulty(100, 1000, "Easy", 0, "graveyard"),
        difficulty(100, 1001, "Hard", 0, "graveyard"),
    ];
    let mut grown = maps.clone();
    grown.push(difficulty(100, 1002, "New Original", 0, "graveyard"));
    server.add_set(100, metadata(100, "graveyard", grown)).await;
    let fixture = MutationFixture::new("effective_archive_growth", server.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (staff, _, _, cookie) = actor(&state, "EffectiveArchiveGrowth", &["beatmaps.moderate"]).await;
        let mut members = Vec::new();
        for map in &maps {
            members.push(store_official(&state, 100, map, map["checksum"].as_str().unwrap()).await);
        }
        let archive = uuid::Uuid::new_v4();
        let mut hashes = maps.iter().map(|map| map["checksum"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        hashes.sort();
        let mut tx = state.db.begin().await.unwrap();
        sqlx::query("INSERT INTO official_beatmapset_archives(official_set_id,id,archive_path,sha256,byte_length,member_md5,uploaded_by) VALUES(100,$1,$2,$3,1,$4,$5)")
            .bind(archive).bind(format!("official-package-{archive}/package.osz")).bind("0".repeat(64)).bind(hashes).bind(staff).execute(&mut *tx).await.unwrap();
        for member in members {
            sqlx::query("INSERT INTO official_beatmapset_archive_members(archive_id,beatmap_id) VALUES($1,$2)")
                .bind(archive).bind(member).execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
        let before = snapshot(&state).await;
        let (status, value) = refresh(&app, &cookie, 100, json!({"reason":"Refresh the original set"})).await;
        assert_eq!(status, StatusCode::CONFLICT, "an archive with two immutable originals cannot publish a three-original downloadable set: {value}");
        assert_eq!(snapshot(&state).await, before, "a failed full manifest check cannot change content, catalogue membership, audit or epoch");
        assert_eq!(before["beatmap_catalogue_metadata"], json!([]));
        assert_eq!(file_count(&state.maps_root).await, 0);
    }).await;
    fixture.finish(result).await;
}
