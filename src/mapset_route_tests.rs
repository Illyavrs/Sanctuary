//! Canonical set URL fixtures own a disposable PG17 schema and synthetic maps.
use crate::{
    AppState, Router,
    beatmap_availability_tests::change,
    maps::{
        map_mutation_tests::{MULTIPART, MutationFixture, actor, multipart, osu_bytes, request},
        map_package_tests::{assert_canonical_archive, files_snapshot},
    },
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use std::io::{Cursor, Read, Write};
use tower::ServiceExt;

async fn get_bytes(app: &Router, path: &str) -> (StatusCode, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

async fn get(app: &Router, path: &str) -> (StatusCode, Value) {
    let (status, bytes) = get_bytes(app, path).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn upload(app: &Router, cookie: &str, filename: &str, bytes: &[u8]) -> Value {
    let (status, value) = request(
        app,
        "/api/v1/beatmaps/upload",
        Some(cookie),
        false,
        MULTIPART,
        multipart(filename, bytes),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}

fn package() -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for mode in 0..4 {
        let bytes = String::from_utf8(osu_bytes("Canonical route package"))
            .unwrap()
            .replace("Mode:0", &format!("Mode:{mode}"))
            .replace("Version:Easy", &format!("Version:Mode {mode}"));
        writer
            .start_file(
                format!("mode-{mode}.osu"),
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        writer.write_all(bytes.as_bytes()).unwrap();
    }
    writer
        .start_file("readme.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    writer
        .write_all(b"owned unchanged route fixture asset")
        .unwrap();
    writer.finish().unwrap().into_inner()
}

async fn snapshot(state: &AppState) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('maps',(SELECT COALESCE(jsonb_agg(to_jsonb(b) ORDER BY id),'[]'::jsonb) FROM beatmaps b),'packages',(SELECT COALESCE(jsonb_agg(to_jsonb(p) ORDER BY id),'[]'::jsonb) FROM beatmap_packages p),'aliases',(SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY filename,beatmap_id),'[]'::jsonb) FROM beatmap_filename_aliases a),'distributions',(SELECT COALESCE(jsonb_agg(to_jsonb(d) ORDER BY package_id),'[]'::jsonb) FROM local_beatmap_distributions d),'members',(SELECT COALESCE(jsonb_agg(to_jsonb(m) ORDER BY beatmap_id),'[]'::jsonb) FROM local_beatmap_distribution_members m),'availability',(SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY native_set_id),'[]'::jsonb) FROM beatmapset_availability a),'audits',(SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY id),'[]'::jsonb) FROM audit_events a),'updates',(SELECT COALESCE(jsonb_agg(to_jsonb(u) ORDER BY id),'[]'::jsonb) FROM lazer_beatmap_updates u))")
        .fetch_one(&state.db).await.unwrap()
}

fn assert_public(value: &Value) {
    for field in ["osu_path", "archive_path"] {
        assert!(value.get(field).is_none());
    }
    for field in ["sha256", "member_md5", "osu_path", "archive_path"] {
        assert!(value["package"].get(field).is_none());
    }
    for field in ["reason", "actor_id"] {
        assert!(value["availability"].get(field).is_none());
    }
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn canonical_local_set_url_binds_set_difficulty_mode_and_delivered_checksum() {
    let fixture = MutationFixture::new("mapset_selection", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (_, _, _, cookie) = actor(&state, "MapsetSelection", &["beatmaps.upload"]).await;
        let original = package();
        let uploaded = upload(&app, &cookie, "route.osz", &original).await;
        let other = upload(
            &app,
            &cookie,
            "other.osu",
            &osu_bytes("Other canonical route set"),
        )
        .await;
        let set = uploaded["package"]["native_set_id"].as_i64().unwrap();
        let other_set = other["package"]["native_set_id"].as_i64().unwrap();
        let items = uploaded["items"].as_array().unwrap();
        let first = items
            .iter()
            .min_by_key(|map| map["id"].as_i64().unwrap())
            .unwrap();
        let endpoint = format!("/api/v1/beatmapsets/{set}");
        let before = snapshot(&state).await;
        let files = files_snapshot(&state.maps_root).await;
        let (status, value) = get(&app, &endpoint).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["id"], first["id"]);
        assert_eq!(
            value["md5"], first["md5"],
            "GET alone does not materialize a download"
        );
        assert_eq!(snapshot(&state).await, before);
        assert_eq!(files_snapshot(&state.maps_root).await, files);

        let (status, downloaded) =
            get_bytes(&app, uploaded["package"]["download_url"].as_str().unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_canonical_archive(&state, &original, &downloaded).await;
        let mut archive = zip::ZipArchive::new(Cursor::new(&downloaded)).unwrap();
        let before_reads = snapshot(&state).await;
        let files = files_snapshot(&state.maps_root).await;
        for mode in 0..4 {
            let map = items.iter().find(|map| map["mode"] == mode).unwrap();
            let id = map["id"].as_i64().unwrap();
            let native = map["native_id"].as_i64().unwrap();
            let (status, value) =
                get(&app, &format!("{endpoint}?difficulty={native}&mode={mode}")).await;
            assert_eq!(status, StatusCode::OK, "{value}");
            assert_eq!(value, get(&app, &format!("/api/v1/beatmaps/{id}")).await.1);
            assert_eq!(value["native_set_id"], set);
            assert_eq!(value["native_id"], native);
            assert_eq!(value["mode"], mode);
            let mut chart = Vec::new();
            archive
                .by_name(&format!("mode-{mode}.osu"))
                .unwrap()
                .read_to_end(&mut chart)
                .unwrap();
            assert_eq!(value["md5"], format!("{:x}", md5::compute(&chart)));
            assert_ne!(value["md5"], map["md5"]);
            assert_public(&value);
            assert_eq!(
                get(&app, &format!("{endpoint}?mode={mode}")).await.1["id"],
                id
            );
            assert_eq!(
                get(
                    &app,
                    &format!("{endpoint}?difficulty={native}&mode={}", (mode + 1) % 4)
                )
                .await
                .0,
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                get(
                    &app,
                    &format!("/api/v1/beatmapsets/{other_set}?difficulty={native}")
                )
                .await
                .0,
                StatusCode::NOT_FOUND
            );
        }
        let other_native = other["items"][0]["native_id"].as_i64().unwrap();
        assert_eq!(
            get(&app, &format!("{endpoint}?difficulty={other_native}"))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get(&app, "/api/v1/beatmapsets/2147483647").await.0,
            StatusCode::NOT_FOUND
        );
        for path in [
            "/api/v1/beatmapsets/0".into(),
            "/api/v1/beatmapsets/-1".into(),
            "/api/v1/beatmapsets/1999999999".into(),
            "/api/v1/beatmapsets/2147483648".into(),
            format!("{endpoint}?difficulty=0"),
            format!("{endpoint}?difficulty=-1"),
            format!("{endpoint}?difficulty=2147483648"),
            format!("{endpoint}?mode=-1"),
            format!("{endpoint}?mode=4"),
            format!("{endpoint}?mode=word"),
        ] {
            assert_eq!(get(&app, &path).await.0, StatusCode::BAD_REQUEST, "{path}");
        }
        assert_eq!(
            snapshot(&state).await,
            before_reads,
            "successful and rejected public resolution writes no state"
        );
        assert_eq!(files_snapshot(&state.maps_root).await, files);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn canonical_local_set_url_obeys_withdrawal_and_never_exposes_staff_reason() {
    let fixture = MutationFixture::new("mapset_withdrawal", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (_, _, _, cookie) = actor(
            &state,
            "MapsetWithdrawal",
            &["beatmaps.upload", "beatmaps.moderate", "admin.view"],
        )
        .await;
        let uploaded = upload(
            &app,
            &cookie,
            "withdraw.osu",
            &osu_bytes("Withdrawn canonical route"),
        )
        .await;
        let set = uploaded["package"]["native_set_id"].as_i64().unwrap();
        let native = uploaded["items"][0]["native_id"].as_i64().unwrap();
        let endpoint = format!("/api/v1/beatmapsets/{set}");
        assert_eq!(get(&app, &endpoint).await.0, StatusCode::OK);
        assert_eq!(
            change(
                &app,
                &cookie,
                set,
                true,
                0,
                "Private route withdrawal reason"
            )
            .await
            .0,
            StatusCode::OK
        );
        let before = snapshot(&state).await;
        let files = files_snapshot(&state.maps_root).await;
        for path in [
            endpoint.clone(),
            format!("{endpoint}?difficulty={native}&mode=0"),
        ] {
            let (status, value) = get(&app, &path).await;
            assert_eq!(status, StatusCode::GONE);
            assert_eq!(value["error"]["code"], "beatmap_withdrawn");
            assert!(value.get("package").is_none());
            assert!(
                !value
                    .to_string()
                    .contains("Private route withdrawal reason")
            );
            assert!(!value.to_string().contains("actor_id"));
        }
        assert_eq!(snapshot(&state).await, before);
        assert_eq!(files_snapshot(&state.maps_root).await, files);
        assert_eq!(
            change(&app, &cookie, set, false, 1, "Restore route fixture")
                .await
                .0,
            StatusCode::OK
        );
        let (status, restored) = get(&app, &format!("{endpoint}?difficulty={native}&mode=0")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(restored["availability"]["revision"], 2);
        assert_eq!(restored["availability"]["withdrawn"], false);
        assert_public(&restored);
        assert_eq!(files_snapshot(&state.maps_root).await, files);
    })
    .await;
    fixture.finish(result).await;
}
