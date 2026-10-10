//! Exact native checksum authority for canonical maps and retained replays.
use super::*;
use crate::maps::map_mutation_tests::{
    MULTIPART, MutationFixture, actor, multipart, osu_bytes, request,
};
use axum::http::StatusCode;
use sha2::{Digest, Sha256};
use std::io::{Cursor, Write};

const ORIGINAL: &str = "0123456789abcdef0123456789abcdef";
const DERIVED: &str = "abcdef0123456789abcdef0123456789";

fn input(map: i64) -> Submission {
    serde_json::from_value(json!({
        "beatmap_id": map, "mode": 0, "score": 1234, "accuracy": 100.0,
        "max_combo": 2, "mods": 0, "passed": true, "hit_counts": {"n300": 2}
    }))
    .unwrap()
}

fn saved(input: &Submission, checksum: Option<&str>) -> Value {
    json!({
        "user_id": 17, "beatmap_id": input.beatmap_id, "mode": input.mode,
        "score_variant": "vanilla", "client": "stable", "score": input.score,
        "accuracy": 100.0, "max_combo": input.max_combo, "mods": input.mods,
        "passed": input.passed, "hit_counts": input.hit_counts,
        "api_mods": null, "native_submission": null, "client_beatmap_md5": checksum
    })
}

#[test]
fn checksum_normalization_requires_exact_hex_and_preserves_original_fallback() {
    assert_eq!(
        normalized_client_checksum(None, ORIGINAL).unwrap(),
        ORIGINAL
    );
    assert_eq!(
        normalized_client_checksum(Some(&DERIVED.to_ascii_uppercase()), ORIGINAL).unwrap(),
        DERIVED
    );
    for invalid in [
        "",
        "0",
        " 0123456789abcdef0123456789abcdef",
        "g123456789abcdef0123456789abcdef",
    ] {
        assert!(normalized_client_checksum(Some(invalid), ORIGINAL).is_err());
    }
    assert!(normalized_client_checksum(None, "").is_err());
}

#[test]
fn historical_null_checksum_never_means_a_preferred_distribution_alias() {
    let mut original = input(23);
    let historical = saved(&original, None);
    assert!(
        ensure_retained_input(
            &historical,
            17,
            &original,
            "stable",
            ScoreVariant::Vanilla,
            ORIGINAL
        )
        .is_ok()
    );
    original.client_beatmap_md5 = Some(ORIGINAL.to_ascii_uppercase());
    assert!(
        ensure_retained_input(
            &historical,
            17,
            &original,
            "stable",
            ScoreVariant::Vanilla,
            ORIGINAL
        )
        .is_ok()
    );
    original.client_beatmap_md5 = Some(DERIVED.into());
    let error = ensure_retained_input(
        &historical,
        17,
        &original,
        "stable",
        ScoreVariant::Vanilla,
        ORIGINAL,
    )
    .unwrap_err();
    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(error.1, "ingestion_conflict");
}

#[test]
fn retained_checksum_cannot_switch_between_original_and_derived_on_retry() {
    let mut retry = input(23);
    retry.client_beatmap_md5 = Some(DERIVED.to_ascii_uppercase());
    let retained = saved(&retry, Some(DERIVED));
    assert!(
        ensure_retained_input(
            &retained,
            17,
            &retry,
            "stable",
            ScoreVariant::Vanilla,
            ORIGINAL
        )
        .is_ok()
    );
    for checksum in [
        None,
        Some(ORIGINAL),
        Some("ffffffffffffffffffffffffffffffff"),
        Some("invalid"),
    ] {
        retry.client_beatmap_md5 = checksum.map(str::to_owned);
        assert_eq!(
            ensure_retained_input(
                &retained,
                17,
                &retry,
                "stable",
                ScoreVariant::Vanilla,
                ORIGINAL
            )
            .unwrap_err()
            .0,
            StatusCode::CONFLICT
        );
    }
    retry.client_beatmap_md5 = Some(DERIVED.into());
    retry.beatmap_id += 1;
    assert!(
        ensure_retained_input(
            &retained,
            17,
            &retry,
            "stable",
            ScoreVariant::Vanilla,
            ORIGINAL
        )
        .is_err()
    );
    retry.beatmap_id -= 1;
    assert!(
        ensure_retained_input(
            &retained,
            18,
            &retry,
            "stable",
            ScoreVariant::Vanilla,
            ORIGINAL
        )
        .is_err()
    );
    let mut forged = retained;
    for value in [json!("not-an-md5"), json!(23), json!([]), json!({})] {
        forged["client_beatmap_md5"] = value;
        assert!(
            ensure_retained_input(
                &forged,
                17,
                &retry,
                "stable",
                ScoreVariant::Vanilla,
                ORIGINAL
            )
            .is_err()
        );
    }
}

fn package_bytes(chart: &[u8]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    zip.start_file("fixture.osu", options).unwrap();
    zip.write_all(chart).unwrap();
    zip.start_file("fixture-audio.bin", options).unwrap();
    zip.write_all(b"owned synthetic audio fixture").unwrap();
    zip.finish().unwrap().into_inner()
}

async fn local_distribution(
    state: &AppState,
    owner: i64,
) -> (i64, i64, String, String, String, Vec<u8>, Vec<u8>) {
    let original = String::from_utf8(osu_bytes("Checksum original"))
        .unwrap()
        .replace(
            "[General]\n",
            "[General]\nAudioFilename:fixture-audio.bin\n",
        )
        .into_bytes();
    let original_md5 = format!("{:x}", md5::compute(&original));
    let archive = package_bytes(&original);
    let package = Uuid::new_v4();
    let original_path = format!("{package}/{original_md5}.osu");
    let archive_path = format!("{package}/package.osz");
    tokio::fs::create_dir(state.maps_root.join(package.to_string()))
        .await
        .unwrap();
    tokio::fs::write(state.maps_root.join(&original_path), &original)
        .await
        .unwrap();
    tokio::fs::write(state.maps_root.join(&archive_path), &archive)
        .await
        .unwrap();
    let mut tx = state.db.begin().await.unwrap();
    let set: i64 = sqlx::query_scalar("INSERT INTO beatmap_packages(id,uploaded_by,kind,sha256,byte_length,member_md5,archive_path) VALUES($1,$2,'osz',$3,$4,ARRAY[$5],$6) RETURNING native_set_id")
        .bind(package).bind(owner).bind(format!("{:x}", Sha256::digest(&archive)))
        .bind(archive.len() as i64).bind(&original_md5).bind(&archive_path).fetch_one(&mut *tx).await.unwrap();
    let (map, native): (i64, i64) = sqlx::query_as("INSERT INTO beatmaps(title,artist,creator,version,mode,stars,max_combo,md5,osu_path,source,uploaded_by,archive_path,local_package_id) VALUES('Checksum original','Illyaverse','Tests','Easy',0,1,2,$1,$2,'local',$3,$4,$5) RETURNING id,native_id")
        .bind(&original_md5).bind(&original_path).bind(owner).bind(&archive_path).bind(package).fetch_one(&mut *tx).await.unwrap();
    let derived = String::from_utf8(original.clone())
        .unwrap()
        .replace(
            "[Metadata]\n",
            &format!("[Metadata]\nBeatmapID:{native}\nBeatmapSetID:{set}\n"),
        )
        .into_bytes();
    let derived_md5 = format!("{:x}", md5::compute(&derived));
    assert_ne!(original_md5, derived_md5);
    let derived_archive = package_bytes(&derived);
    let distribution = Uuid::new_v4();
    let derived_path = format!("distribution-{distribution}/{derived_md5}.osu");
    let derived_archive_path = format!("distribution-{distribution}/package.osz");
    tokio::fs::create_dir(state.maps_root.join(format!("distribution-{distribution}")))
        .await
        .unwrap();
    tokio::fs::write(state.maps_root.join(&derived_path), &derived)
        .await
        .unwrap();
    tokio::fs::write(
        state.maps_root.join(&derived_archive_path),
        &derived_archive,
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO local_beatmap_distributions(package_id,id,native_set_id,format_version,kind,original_sha256,original_byte_length,archive_path,sha256,byte_length) VALUES($1,$2,$3,1,'osz',$4,$5,$6,$7,$8)")
        .bind(package).bind(distribution).bind(set).bind(format!("{:x}", Sha256::digest(&archive))).bind(archive.len() as i64)
        .bind(&derived_archive_path).bind(format!("{:x}", Sha256::digest(&derived_archive))).bind(derived_archive.len() as i64).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO local_beatmap_distribution_members(distribution_id,beatmap_id,original_md5,derived_md5,native_id,native_set_id,osu_path,sha256,byte_length) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(distribution).bind(map).bind(&original_md5).bind(&derived_md5).bind(native).bind(set).bind(derived_path)
        .bind(format!("{:x}", Sha256::digest(&derived))).bind(derived.len() as i64).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    (
        map,
        set,
        original_md5,
        derived_md5,
        archive_path,
        original,
        archive,
    )
}

async fn native_ingest(app: &axum::Router, body: &Value) -> (StatusCode, Value) {
    request(
        app,
        "/api/v1/internal/scores",
        None,
        true,
        "application/json",
        body.to_string().into_bytes(),
    )
    .await
}

fn legacy_replay(checksum: &str, username: &str) -> Vec<u8> {
    let mut writer = resonance_protocol::stable::Writer::new();
    writer
        .u8(0)
        .i32(20261010)
        .string(checksum)
        .unwrap()
        .string(username)
        .unwrap()
        .string("")
        .unwrap();
    for count in [2, 0, 0, 0, 0, 0] {
        writer.u16(count);
    }
    writer
        .i32(1234)
        .u16(2)
        .boolean(true)
        .u32(0)
        .string("")
        .unwrap()
        .i64(639269732961234567)
        .i32(3);
    let mut bytes = writer.into_bytes();
    bytes.extend_from_slice(&[1, 2, 3]);
    bytes
}

fn replay_bytes(
    id: i64,
    owner: i64,
    username: &str,
    checksum: &str,
    native: Option<&Value>,
) -> Vec<u8> {
    let Some(native) = native else {
        return legacy_replay(checksum, username);
    };
    let submission = serde_json::from_value(native.clone()).unwrap();
    let binding = resonance_protocol::lazer_replay::ReplayScore {
        online_id: id,
        user_id: owner,
        username,
        beatmap_md5: checksum,
        client_version: "owned-checksum-fixture",
        mods_bits: 0,
        timestamp_ticks: 639269732961234567,
        submission: &submission,
    };
    resonance_protocol::lazer_replay::encode(&binding, b"0|256|192|0,100|256|192|1,-12345|0|0|0")
        .unwrap()
}

async fn replay_binding(state: &AppState, id: i64) -> ReplayUploadBinding {
    let row = sqlx::query("SELECT s.*,COALESCE(s.client_beatmap_md5,b.md5) replay_beatmap_md5,u.username,s.native_submission replay_submission FROM scores s JOIN beatmaps b ON b.id=s.beatmap_id JOIN users u ON u.id=s.user_id WHERE s.id=$1")
        .bind(id).fetch_one(&state.db).await.unwrap();
    ReplayUploadBinding::from_row(&row)
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17 and full schema including 0028"]
async fn native_checksum_admission_retains_original_and_derived_replay_bindings() {
    let database_url = std::env::var("ILLYA_TEST_DATABASE_URL")
        .expect("owned loopback PostgreSQL fixture URL is required");
    let parsed_url =
        reqwest::Url::parse(&database_url).expect("owned fixture database URL must parse");
    assert!(
        matches!(parsed_url.scheme(), "postgres" | "postgresql")
            && matches!(
                parsed_url.host_str(),
                Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
            ),
        "checksum fixture requires an owned loopback PostgreSQL URL"
    );
    let fixture = MutationFixture::new("score_checksum", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    // The assertion task is joined before fixture.finish removes its owned
    // schema, connections and files, including after an assertion panic.
    let result = tokio::spawn(async move {
        let (owner, _, _, cookie) = actor(&state, "ChecksumOwner", &[]).await;
        let (_, _, _, stranger) = actor(&state, "ChecksumStranger", &[]).await;
        let (map, set, original_md5, derived_md5, archive_path, original, archive) = local_distribution(&state, owner).await;
        assert_eq!(sqlx::query_scalar::<_, String>("SELECT illya_beatmap_preferred_checksum($1)").bind(map).fetch_one(&state.db).await.unwrap(), derived_md5);
        let mut retained = Vec::new();
        for client in ["stable", "lazer"] {
            for (variant, checksum) in [("original", &original_md5), ("derived", &derived_md5)] {
                let mut body = json!({"user_id":owner,"beatmap_id":map,"mode":0,"score":1234,"accuracy":100.0,"max_combo":2,"mods":0,"passed":true,"hit_counts":{"n300":2},"client":client,"ingestion_key":format!("checksum-{client}-{variant}"),"client_beatmap_md5":checksum.to_ascii_uppercase()});
                if client == "lazer" {
                    body["api_mods"] = json!([]);
                    body["native_submission"] = json!({"ruleset_id":0,"passed":true,"total_score":1234,"total_score_without_mods":1234,"accuracy":1.0,"max_combo":2,"rank":"X","mods":[],"statistics":{"great":2},"maximum_statistics":{"great":2},"pauses":[]});
                }
                let (status, value) = native_ingest(&app, &body).await;
                assert_eq!(status, StatusCode::OK, "{client}/{variant}: {value}");
                assert_eq!(value["beatmap_id"], map);
                assert_eq!(value["client_beatmap_md5"], *checksum);
                retained.push((body, value["id"].as_i64().unwrap(), checksum.clone()));
            }
        }
        let source = retained[0].1;
        let historical: i64 = sqlx::query_scalar("INSERT INTO scores(user_id,beatmap_id,mode,score,accuracy,max_combo,mods,pp,passed,hit_counts,client,ingestion_key,api_mods,score_variant) SELECT user_id,beatmap_id,mode,score,accuracy,max_combo,mods,pp,passed,hit_counts,client,'checksum-historical',api_mods,score_variant FROM scores WHERE id=$1 RETURNING id")
            .bind(source).fetch_one(&state.db).await.unwrap();
        let mut historical_input = retained[0].0.clone();
        historical_input["ingestion_key"] = json!("checksum-historical");
        historical_input.as_object_mut().unwrap().remove("client_beatmap_md5");
        assert_eq!(native_ingest(&app, &historical_input).await.1["id"], historical);
        assert!(sqlx::query_scalar::<_, Option<String>>("SELECT client_beatmap_md5 FROM scores WHERE id=$1").bind(historical).fetch_one(&state.db).await.unwrap().is_none());
        retained.push((historical_input, historical, original_md5.clone()));

        let wrong_bytes = osu_bytes("Different canonical map");
        tokio::fs::write(state.maps_root.join("wrong.osu"), &wrong_bytes).await.unwrap();
        let wrong_map: i64 = sqlx::query_scalar("INSERT INTO beatmaps(title,artist,creator,version,mode,stars,max_combo,md5,osu_path) VALUES('Wrong','Fixture','Tests','Easy',0,1,2,$1,'wrong.osu') RETURNING id")
            .bind(format!("{:x}", md5::compute(wrong_bytes))).fetch_one(&state.db).await.unwrap();
        for (name, beatmap, checksum) in [("forged", map, "ffffffffffffffffffffffffffffffff"), ("wrong-map", wrong_map, derived_md5.as_str()), ("malformed", map, "invalid")] {
            let mut body = retained[0].0.clone();
            body["ingestion_key"] = json!(format!("checksum-reject-{name}"));
            body["beatmap_id"] = json!(beatmap);
            body["client_beatmap_md5"] = json!(checksum);
            assert_eq!(native_ingest(&app, &body).await.0, StatusCode::BAD_REQUEST, "{name}");
        }
        let public_forgery = json!({"beatmap_id":map,"mode":0,"score":1234,"accuracy":100.0,"max_combo":2,"mods":0,"passed":true,"hit_counts":{"n300":2},"client_beatmap_md5":derived_md5});
        assert_eq!(request(&app, "/api/v1/scores", Some(&cookie), false, "application/json", public_forgery.to_string().into_bytes()).await.0, StatusCode::BAD_REQUEST);

        for (body, id, checksum) in &retained {
            let other = if checksum == &original_md5 { &derived_md5 } else { &original_md5 };
            let native = body.get("native_submission");
            let bytes = replay_bytes(*id, owner, "ChecksumOwner", checksum, native);
            let wrong = replay_bytes(*id, owner, "ChecksumOwner", other, native);
            let binding = replay_binding(&state, *id).await;
            assert_eq!(binding.md5, *checksum);
            assert!(binding.validate(*id, &bytes).is_ok());
            assert_eq!(binding.validate(*id, &wrong).unwrap_err().0, StatusCode::BAD_REQUEST);
            let path = format!("/api/v1/scores/{id}/replay");
            assert_eq!(request(&app, &path, Some(&stranger), false, MULTIPART, multipart("fixture.osr", &wrong)).await.0, StatusCode::FORBIDDEN);
            assert_eq!(request(&app, &path, Some(&cookie), false, MULTIPART, multipart("fixture.osr", &wrong)).await.0, StatusCode::BAD_REQUEST);
            let (status, value) = request(&app, &path, Some(&cookie), false, MULTIPART, multipart("fixture.osr", &bytes)).await;
            assert_eq!(status, StatusCode::OK, "{value}");
            let (status, value) = request(&app, &path, Some(&cookie), false, MULTIPART, multipart("fixture.osr", &bytes)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(value["idempotent"], true);
            let before: Value = sqlx::query_scalar("SELECT to_jsonb(s) FROM scores s WHERE id=$1").bind(id).fetch_one(&state.db).await.unwrap();
            assert_eq!(native_ingest(&app, body).await.1["id"], *id);
            let mut switched = body.clone();
            switched["client_beatmap_md5"] = json!(other);
            let (status, error) = native_ingest(&app, &switched).await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(error["error"]["code"], "ingestion_conflict");
            let direct = sqlx::query("UPDATE scores SET client_beatmap_md5=$2 WHERE id=$1").bind(id).bind(other).execute(&state.db).await.unwrap_err();
            assert_eq!(direct.as_database_error().unwrap().code().as_deref(), Some("23514"));
            assert_eq!(sqlx::query_scalar::<_, Value>("SELECT to_jsonb(s) FROM scores s WHERE id=$1").bind(id).fetch_one(&state.db).await.unwrap(), before);
            let stored = before["replay_path"].as_str().unwrap();
            assert_eq!(tokio::fs::read(state.replay_root.join(stored)).await.unwrap(), bytes);
        }
        // The owner receives the incomplete native metadata error; another
        // player must still receive only forbidden, before replay validation.
        let incomplete: i64 = sqlx::query_scalar("INSERT INTO scores(user_id,beatmap_id,mode,score,accuracy,max_combo,mods,pp,passed,hit_counts,client,client_beatmap_md5) VALUES($1,$2,0,1234,100,2,0,0,true,'{\"n300\":2}','lazer',$3) RETURNING id")
            .bind(owner).bind(map).bind(&derived_md5).fetch_one(&state.db).await.unwrap();
        let incomplete_path = format!("/api/v1/scores/{incomplete}/replay");
        let bytes = legacy_replay(&derived_md5, "ChecksumOwner");
        assert_eq!(request(&app, &incomplete_path, Some(&stranger), false, MULTIPART, multipart("fixture.osr", &bytes)).await.0, StatusCode::FORBIDDEN);
        assert_eq!(request(&app, &incomplete_path, Some(&cookie), false, MULTIPART, multipart("fixture.osr", &bytes)).await.0, StatusCode::BAD_REQUEST);
        let mut tx = state.db.begin().await.unwrap();
        crate::beatmap_availability::lock_exclusive_set(&mut tx, set).await.unwrap();
        sqlx::query("INSERT INTO beatmapset_availability(native_set_id,withdrawn,revision,reason,actor_id) VALUES($1,true,1,'Owned checksum retry fixture',$2)")
            .bind(set).bind(owner).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        for (body, id, checksum) in &retained {
            let before: Value = sqlx::query_scalar("SELECT to_jsonb(s) FROM scores s WHERE id=$1").bind(id).fetch_one(&state.db).await.unwrap();
            let (status, value) = native_ingest(&app, body).await;
            assert_eq!(status, StatusCode::OK, "{value}");
            assert_eq!(value["id"], *id);
            let mut switched = body.clone();
            switched["client_beatmap_md5"] = json!(if checksum == &original_md5 { &derived_md5 } else { &original_md5 });
            assert_eq!(native_ingest(&app, &switched).await.0, StatusCode::CONFLICT);
            assert_eq!(sqlx::query_scalar::<_, Value>("SELECT to_jsonb(s) FROM scores s WHERE id=$1").bind(id).fetch_one(&state.db).await.unwrap(), before);
            assert_eq!(tokio::fs::read(state.replay_root.join(before["replay_path"].as_str().unwrap())).await.unwrap(), replay_bytes(*id, owner, "ChecksumOwner", checksum, body.get("native_submission")));
        }
        assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM scores").fetch_one(&state.db).await.unwrap(), 6);
        assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM scores WHERE beatmap_id=$1").bind(map).fetch_one(&state.db).await.unwrap(), 6);
        assert_eq!(sqlx::query_scalar::<_, String>("SELECT md5 FROM beatmaps WHERE id=$1").bind(map).fetch_one(&state.db).await.unwrap(), original_md5);
        let original_path: String = sqlx::query_scalar("SELECT osu_path FROM beatmaps WHERE id=$1").bind(map).fetch_one(&state.db).await.unwrap();
        assert_eq!(tokio::fs::read(state.maps_root.join(original_path)).await.unwrap(), original);
        assert_eq!(tokio::fs::read(state.maps_root.join(archive_path)).await.unwrap(), archive);
    }).await;
    fixture.finish(result).await;
}
