//! Derived downloads use an owned PG17 schema and synthetic local packages only.
use crate::{
    AppState, Router,
    beatmap_availability_tests::change,
    local_distribution,
    maps::map_mutation_tests::{
        MULTIPART, MutationFixture, actor, multipart, osu_bytes, request, wait_for_lock,
    },
};
use axum::http::StatusCode;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use std::{collections::BTreeMap, io::Cursor, path::Path};
use uuid::Uuid;

fn difficulty(title: &str, version: &str) -> Vec<u8> {
    String::from_utf8(osu_bytes(title))
        .unwrap()
        .replace("Mode:0", "AudioFilename:../assets/audio.mp3\nMode:0")
        .replace(
            "Version:Easy",
            &format!("Version:{version}\nBeatmapID:-1\nBeatmapSetID:-1"),
        )
        .replace(
            "[Difficulty]",
            "[Events]\n0,0,\"../images/background.png\",0,0\n[Difficulty]",
        )
        .into_bytes()
}

fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut writer, bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn entries(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut result = BTreeMap::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
        assert!(result.insert(entry.name().to_owned(), bytes).is_none());
    }
    result
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
    assert_eq!(status, StatusCode::OK, "{filename}: {value}");
    value
}

fn ids(upload: &Value) -> Vec<i64> {
    let mut result = upload["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_i64().unwrap())
        .collect::<Vec<_>>();
    result.sort_unstable();
    result
}

fn package_id(upload: &Value) -> Uuid {
    Uuid::parse_str(upload["package"]["id"].as_str().unwrap()).unwrap()
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, PartialEq)]
struct SourceSnapshot {
    maps: Value,
    packages: Value,
    aliases: Value,
    audits: Value,
    updates: Value,
}

fn assert_materialization_event(before: &SourceSnapshot, after: &SourceSnapshot, set_id: i64) {
    assert_eq!(after.maps, before.maps);
    assert_eq!(after.packages, before.packages);
    assert_eq!(after.aliases, before.aliases);
    assert_eq!(after.audits, before.audits);
    let previous = before.updates.as_array().unwrap();
    let current = after.updates.as_array().unwrap();
    assert_eq!(
        current.len(),
        previous.len() + 1,
        "one committed native cursor event per complete distribution"
    );
    assert_eq!(&current[..previous.len()], previous.as_slice());
    assert_eq!(current.last().unwrap()["beatmap_set_id"], set_id);
}

async fn source_snapshot(db: &PgPool) -> SourceSnapshot {
    SourceSnapshot {
        maps: sqlx::query_scalar(
            "SELECT COALESCE(jsonb_agg(to_jsonb(b) ORDER BY id),'[]'::jsonb) FROM beatmaps b",
        )
        .fetch_one(db)
        .await
        .unwrap(),
        packages: sqlx::query_scalar(
            "SELECT COALESCE(jsonb_agg(to_jsonb(p) ORDER BY id),'[]'::jsonb) FROM beatmap_packages p",
        )
        .fetch_one(db)
        .await
        .unwrap(),
        aliases: sqlx::query_scalar("SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY filename COLLATE \"C\",beatmap_id),'[]'::jsonb) FROM beatmap_filename_aliases a")
            .fetch_one(db).await.unwrap(),
        audits: sqlx::query_scalar(
            "SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY id),'[]'::jsonb) FROM audit_events a",
        )
        .fetch_one(db)
        .await
        .unwrap(),
        updates: sqlx::query_scalar(
            "SELECT COALESCE(jsonb_agg(to_jsonb(u) ORDER BY id),'[]'::jsonb) FROM lazer_beatmap_updates u",
        )
        .fetch_one(db)
        .await
        .unwrap(),
    }
}

async fn derived_snapshot(db: &PgPool) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('distributions',(SELECT COALESCE(jsonb_agg(to_jsonb(d) ORDER BY package_id),'[]'::jsonb) FROM local_beatmap_distributions d),'members',(SELECT COALESCE(jsonb_agg(to_jsonb(m) ORDER BY beatmap_id),'[]'::jsonb) FROM local_beatmap_distribution_members m))")
        .fetch_one(db).await.unwrap()
}

async fn files_snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut result = BTreeMap::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let mut entries = tokio::fs::read_dir(directory).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            if entry.file_type().await.unwrap().is_dir() {
                directories.push(entry.path());
            } else {
                let path = entry.path();
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                assert!(
                    result
                        .insert(relative, tokio::fs::read(path).await.unwrap())
                        .is_none()
                );
            }
        }
    }
    result
}

fn metadata_id(bytes: &[u8], name: &str) -> i64 {
    let prefix = format!("{name}:");
    let values = std::str::from_utf8(bytes)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .map(|value| value.trim().parse::<i64>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(values.len(), 1, "one canonical {name} entry");
    values[0]
}

fn without_ids(bytes: &[u8]) -> Vec<u8> {
    std::str::from_utf8(bytes)
        .unwrap()
        .split_inclusive('\n')
        .filter(|line| !line.starts_with("BeatmapID:") && !line.starts_with("BeatmapSetID:"))
        .collect::<String>()
        .into_bytes()
}

fn assert_sqlstate(error: &sqlx::Error, expected: &str) {
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some(expected),
        "{error}"
    );
}

async fn insert_distribution(
    tx: &mut Transaction<'_, Postgres>,
    package: Uuid,
    distribution: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO local_beatmap_distributions(package_id,id,native_set_id,format_version,kind,original_sha256,original_byte_length,archive_path,sha256,byte_length) SELECT id,$2,native_set_id,1,kind,sha256,byte_length,CASE WHEN kind='osz' THEN 'distribution-'||$2::uuid::text||'/package.osz' END,CASE WHEN kind='osz' THEN repeat('a',64) END,CASE WHEN kind='osz' THEN 127 END FROM beatmap_packages WHERE id=$1")
        .bind(package).bind(distribution).execute(&mut **tx).await?;
    Ok(())
}

async fn insert_member(
    tx: &mut Transaction<'_, Postgres>,
    distribution: Uuid,
    map: i64,
    checksum: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO local_beatmap_distribution_members(distribution_id,beatmap_id,original_md5,derived_md5,native_id,native_set_id,osu_path,sha256,byte_length) SELECT $1,id,md5,$3,native_id,native_set_id,'distribution-'||$1::uuid::text||'/'||$3||'.osu',repeat('b',64),42 FROM beatmaps WHERE id=$2")
        .bind(distribution).bind(map).bind(checksum).execute(&mut **tx).await?;
    Ok(())
}

async fn insert_distribution_with_orphans(
    tx: &mut Transaction<'_, Postgres>,
    package: Uuid,
    distribution: Uuid,
    orphans: &[String],
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO local_beatmap_distributions(package_id,id,native_set_id,format_version,kind,original_sha256,original_byte_length,archive_path,sha256,byte_length,retained_orphan_md5) SELECT id,$2,native_set_id,1,kind,sha256,byte_length,'distribution-'||$2::uuid::text||'/package.osz',repeat('a',64),127,$3 FROM beatmap_packages WHERE id=$1")
        .bind(package).bind(distribution).bind(orphans).execute(&mut **tx).await?;
    Ok(())
}

async fn seed_official_checksum(
    db: &PgPool,
    official_id: i64,
    checksum: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO beatmaps(official_id,official_set_id,title,artist,creator,version,mode,stars,md5,osu_path,source) VALUES($1,$1,'Checksum collision fixture','Fixture','Fixture','Normal',0,1,$2,'unread-fixture.osu','official') RETURNING id")
        .bind(official_id).bind(checksum).fetch_one(db).await
}

async fn verify_complete_archive(state: &AppState, app: &Router, cookie: &str) -> Value {
    let easy = difficulty("Derived local package", "Easy");
    let hard = difficulty("Derived local package", "Hard");
    let original = archive(&[
        ("maps/hard [owned].osu", &hard),
        ("maps/easy.osu", &easy),
        ("assets/audio.mp3", b"owned synthetic audio bytes"),
        ("images/background.png", b"owned synthetic background bytes"),
        ("readme.txt", b"owned unchanged asset\r\n"),
    ]);
    let created = upload(app, cookie, "original.osz", &original).await;
    let maps = ids(&created);
    assert_eq!(maps.len(), 2);
    let before = source_snapshot(&state.db).await;
    let original_files = files_snapshot(&state.maps_root).await;
    assert_eq!(original_files.len(), 3);
    let (first, concurrent) = tokio::join!(
        local_distribution::file(state, maps[0], true),
        local_distribution::file(state, maps[1], true)
    );
    let distributed = first.unwrap();
    let concurrent = concurrent.unwrap();
    assert_eq!(concurrent.bytes, distributed.bytes);
    assert_eq!(concurrent.sha256, distributed.sha256);
    assert!(distributed.md5.is_none());
    assert_eq!(distributed.sha256, digest(&distributed.bytes));
    assert_ne!(
        distributed.bytes, original,
        "IDs change only distributed content"
    );
    let published_source = source_snapshot(&state.db).await;
    assert_materialization_event(
        &before,
        &published_source,
        created["package"]["native_set_id"].as_i64().unwrap(),
    );
    let derived = derived_snapshot(&state.db).await;
    assert_eq!(derived["distributions"].as_array().unwrap().len(), 1);
    assert_eq!(derived["members"].as_array().unwrap().len(), 2);
    let distribution = &derived["distributions"][0];
    assert_eq!(distribution["package_id"], package_id(&created).to_string());
    assert_eq!(distribution["kind"], "osz");
    assert_eq!(distribution["format_version"], 1);
    assert_eq!(distribution["original_sha256"], digest(&original));
    assert_eq!(distribution["original_byte_length"], original.len());
    assert_eq!(distribution["sha256"], distributed.sha256);
    assert_eq!(distribution["byte_length"], distributed.bytes.len());
    let original_entries = entries(&original);
    let derived_entries = entries(&distributed.bytes);
    assert_eq!(
        original_entries.keys().collect::<Vec<_>>(),
        derived_entries.keys().collect::<Vec<_>>(),
        "all original ZIP entry names survive"
    );
    for (name, original_bytes) in &original_entries {
        let derived_bytes = &derived_entries[name];
        if !name.ends_with(".osu") {
            assert_eq!(derived_bytes, original_bytes, "asset {name}");
            continue;
        }
        assert_eq!(without_ids(derived_bytes), without_ids(original_bytes));
        let checksum = format!("{:x}", md5::compute(derived_bytes));
        let member = derived["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|member| member["derived_md5"] == checksum)
            .expect("every rewritten archive chart is in the committed complete manifest");
        assert_eq!(
            member["original_md5"],
            format!("{:x}", md5::compute(original_bytes))
        );
        assert_ne!(member["derived_md5"], member["original_md5"]);
        assert_eq!(member["sha256"], digest(derived_bytes));
        assert_eq!(member["byte_length"], derived_bytes.len());
        assert_eq!(member["native_id"], metadata_id(derived_bytes, "BeatmapID"));
        assert_eq!(
            member["native_set_id"],
            metadata_id(derived_bytes, "BeatmapSetID")
        );
        assert_eq!(member["native_set_id"], created["package"]["native_set_id"]);
        let id = member["beatmap_id"].as_i64().unwrap();
        let map = before
            .maps
            .as_array()
            .unwrap()
            .iter()
            .find(|map| map["id"] == id)
            .unwrap();
        assert_eq!(member["native_id"], map["native_id"]);
        assert_eq!(member["original_md5"], map["md5"]);
        let selected = local_distribution::file(state, id, false).await.unwrap();
        assert_eq!(selected.bytes, *derived_bytes);
        assert_eq!(selected.md5.as_deref(), Some(checksum.as_str()));
        assert_eq!(selected.sha256, digest(derived_bytes));
        assert_eq!(
            tokio::fs::read(state.maps_root.join(member["osu_path"].as_str().unwrap()))
                .await
                .unwrap(),
            *derived_bytes
        );
        for identity in [member["original_md5"].as_str().unwrap(), checksum.as_str()] {
            assert!(
                sqlx::query_scalar::<_, bool>("SELECT illya_beatmap_checksum_matches($1,$2)")
                    .bind(id)
                    .bind(identity)
                    .fetch_one(&state.db)
                    .await
                    .unwrap()
            );
            let rows: Vec<(i64, bool)> = sqlx::query_as(
                "SELECT beatmap_id,derived FROM beatmap_checksum_identities WHERE md5=$1",
            )
            .bind(identity)
            .fetch_all(&state.db)
            .await
            .unwrap();
            assert_eq!(rows, vec![(id, identity == checksum)]);
        }
        let preferred: String = sqlx::query_scalar("SELECT illya_beatmap_preferred_checksum($1)")
            .bind(id)
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(preferred, checksum);
        assert!(
            !sqlx::query_scalar::<_, bool>("SELECT illya_beatmap_checksum_matches($1,$2)")
                .bind(id)
                .bind(checksum.to_uppercase())
                .fetch_one(&state.db)
                .await
                .unwrap()
        );
        let sibling = *maps.iter().find(|sibling| **sibling != id).unwrap();
        assert!(
            !sqlx::query_scalar::<_, bool>("SELECT illya_beatmap_checksum_matches($1,$2)")
                .bind(sibling)
                .bind(&checksum)
                .fetch_one(&state.db)
                .await
                .unwrap()
        );
    }
    let all_files = files_snapshot(&state.maps_root).await;
    assert_eq!(
        all_files.len(),
        6,
        "one archive and two chart files per source/derived package"
    );
    for (name, bytes) in original_files {
        assert_eq!(all_files.get(&name), Some(&bytes), "source bytes {name}");
    }
    let repeated = local_distribution::file(state, maps[1], true)
        .await
        .unwrap();
    assert_eq!(repeated.bytes, distributed.bytes);
    assert_eq!(repeated.sha256, distributed.sha256);
    assert_eq!(derived_snapshot(&state.db).await, derived);
    assert_eq!(files_snapshot(&state.maps_root).await, all_files);
    let retry = upload(app, cookie, "original.osz", &original).await;
    assert_eq!(retry["idempotent"], true);
    assert_eq!(ids(&retry), maps);
    assert_eq!(source_snapshot(&state.db).await, published_source);
    assert_eq!(derived_snapshot(&state.db).await, derived);
    assert_eq!(files_snapshot(&state.maps_root).await, all_files);
    created
}

async fn verify_namespace_and_immutable_guards(state: &AppState, app: &Router, cookie: &str) {
    let before = derived_snapshot(&state.db).await;
    let original = source_snapshot(&state.db).await;
    let files = files_snapshot(&state.maps_root).await;
    for sql in [
        "UPDATE local_beatmap_distributions SET format_version=format_version",
        "DELETE FROM local_beatmap_distributions",
        "UPDATE local_beatmap_distribution_members SET derived_md5=derived_md5",
        "DELETE FROM local_beatmap_distribution_members",
        "UPDATE beatmaps SET mode=1 WHERE id IN(SELECT beatmap_id FROM local_beatmap_distribution_members)",
        "UPDATE beatmaps SET source='official' WHERE id IN(SELECT beatmap_id FROM local_beatmap_distribution_members)",
    ] {
        assert_sqlstate(
            &sqlx::query(sql).execute(&state.db).await.unwrap_err(),
            "23514",
        );
        assert_eq!(derived_snapshot(&state.db).await, before);
        assert_eq!(source_snapshot(&state.db).await, original);
        assert_eq!(files_snapshot(&state.maps_root).await, files);
    }
    let alias = before["members"][0]["derived_md5"].as_str().unwrap();
    assert_sqlstate(
        &seed_official_checksum(&state.db, 760_001, alias)
            .await
            .unwrap_err(),
        "23505",
    );
    assert_eq!(source_snapshot(&state.db).await, original);
    assert_eq!(derived_snapshot(&state.db).await, before);

    let mismatched = upload(
        app,
        cookie,
        "mode-mismatch.osu",
        &difficulty("Unmaterialized mode mismatch", "Normal"),
    )
    .await;
    let mismatched_id = ids(&mismatched)[0];
    sqlx::query("UPDATE beatmaps SET mode=1 WHERE id=$1")
        .bind(mismatched_id)
        .execute(&state.db)
        .await
        .unwrap();
    let mismatched_source = source_snapshot(&state.db).await;
    let mismatched_files = files_snapshot(&state.maps_root).await;
    let error = local_distribution::file(state, mismatched_id, false)
        .await
        .err()
        .unwrap();
    assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        source_snapshot(&state.db).await,
        mismatched_source,
        "the rejected materializer preserves the intentionally changed mode row"
    );
    assert_eq!(derived_snapshot(&state.db).await, before);
    assert_eq!(
        files_snapshot(&state.maps_root).await,
        mismatched_files,
        "ruleset mismatch creates no staging or aliases"
    );

    let first = difficulty("Database guard package", "Easy");
    let second = difficulty("Database guard package", "Hard");
    let payload = archive(&[("easy.osu", &first), ("hard.osu", &second)]);
    let uploaded = upload(app, cookie, "guards.osz", &payload).await;
    let package = package_id(&uploaded);
    let maps = ids(&uploaded);
    let checksum = format!(
        "{:x}",
        md5::compute(b"owned checksum already stored as original")
    );
    let other = seed_official_checksum(&state.db, 760_002, &checksum)
        .await
        .unwrap();
    let source = source_snapshot(&state.db).await;
    let derived = derived_snapshot(&state.db).await;
    let retained = files_snapshot(&state.maps_root).await;
    let mut tx = state.db.begin().await.unwrap();
    let distribution = Uuid::new_v4();
    insert_distribution(&mut tx, package, distribution)
        .await
        .unwrap();
    assert_sqlstate(
        &insert_member(&mut tx, distribution, maps[0], &checksum)
            .await
            .unwrap_err(),
        "23505",
    );
    tx.rollback().await.unwrap();
    assert!(
        !sqlx::query_scalar::<_, bool>("SELECT illya_beatmap_checksum_matches($1,$2)")
            .bind(maps[0])
            .bind(&checksum)
            .fetch_one(&state.db)
            .await
            .unwrap()
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT illya_beatmap_checksum_matches($1,$2)")
            .bind(other)
            .bind(&checksum)
            .fetch_one(&state.db)
            .await
            .unwrap()
    );
    assert_eq!(source_snapshot(&state.db).await, source);
    assert_eq!(derived_snapshot(&state.db).await, derived);
    assert_eq!(files_snapshot(&state.maps_root).await, retained);

    for member_count in [0, 1] {
        let mut tx = state.db.begin().await.unwrap();
        let distribution = Uuid::new_v4();
        insert_distribution(&mut tx, package, distribution)
            .await
            .unwrap();
        if member_count == 1 {
            let partial_checksum = format!("{:x}", md5::compute(distribution.as_bytes()));
            insert_member(&mut tx, distribution, maps[0], &partial_checksum)
                .await
                .unwrap();
        }
        let staged: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM local_beatmap_distribution_members WHERE distribution_id=$1",
        )
        .bind(distribution)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(staged, member_count);
        assert_sqlstate(&tx.commit().await.unwrap_err(), "23514");
        assert_eq!(source_snapshot(&state.db).await, source);
        assert_eq!(derived_snapshot(&state.db).await, derived);
        assert_eq!(files_snapshot(&state.maps_root).await, retained);
    }
}

async fn verify_standalone_and_withdrawal(
    state: &AppState,
    app: &Router,
    cookie: &str,
    staff: &str,
    archived: &Value,
) {
    let raw = difficulty("Standalone derived chart", "Normal");
    let uploaded = upload(app, cookie, "standalone.osu", &raw).await;
    let map = ids(&uploaded)[0];
    let source = source_snapshot(&state.db).await;
    let preferred_before: String =
        sqlx::query_scalar("SELECT illya_beatmap_preferred_checksum($1)")
            .bind(map)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(preferred_before, format!("{:x}", md5::compute(&raw)));
    let chart = local_distribution::file(state, map, false).await.unwrap();
    assert_eq!(without_ids(&chart.bytes), without_ids(&raw));
    assert_eq!(
        metadata_id(&chart.bytes, "BeatmapID"),
        uploaded["items"][0]["native_id"].as_i64().unwrap()
    );
    assert_eq!(
        metadata_id(&chart.bytes, "BeatmapSetID"),
        uploaded["package"]["native_set_id"].as_i64().unwrap()
    );
    assert_eq!(chart.md5, Some(format!("{:x}", md5::compute(&chart.bytes))));
    assert_eq!(chart.sha256, digest(&chart.bytes));
    let published_source = source_snapshot(&state.db).await;
    assert_materialization_event(
        &source,
        &published_source,
        uploaded["package"]["native_set_id"].as_i64().unwrap(),
    );
    let distribution: Value = sqlx::query_scalar(
        "SELECT to_jsonb(d) FROM local_beatmap_distributions d WHERE package_id=$1",
    )
    .bind(package_id(&uploaded))
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(distribution["kind"], "osu");
    for field in ["archive_path", "sha256", "byte_length"] {
        assert!(distribution[field].is_null());
    }
    assert_eq!(
        local_distribution::file(state, map, true)
            .await
            .err()
            .unwrap()
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(source_snapshot(&state.db).await, published_source);

    let archived_map = ids(archived)[0];
    let archived_set = archived["package"]["native_set_id"].as_i64().unwrap();
    let archived_file = local_distribution::file(state, archived_map, true)
        .await
        .unwrap();
    let before = derived_snapshot(&state.db).await;
    let retained = files_snapshot(&state.maps_root).await;
    let (status, result) = change(app, staff, archived_set, true, 0, "Owned test withdrawal").await;
    assert_eq!(status, StatusCode::OK, "{result}");
    for is_archive in [false, true] {
        let error = local_distribution::file(state, archived_map, is_archive)
            .await
            .err()
            .unwrap();
        assert_eq!(error.0, StatusCode::GONE);
        assert_eq!(error.1, "beatmap_withdrawn");
    }
    assert_eq!(derived_snapshot(&state.db).await, before);
    assert_eq!(files_snapshot(&state.maps_root).await, retained);
    let (status, result) = change(app, staff, archived_set, false, 1, "Owned test restore").await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let restored = local_distribution::file(state, archived_map, true)
        .await
        .unwrap();
    assert_eq!(restored.bytes, archived_file.bytes);
    assert_eq!(derived_snapshot(&state.db).await, before);

    let payload = archive(&[(
        "cold.osu",
        &difficulty("Withdrawn before materialization", "Easy"),
    )]);
    let cold = upload(app, cookie, "withdrawn.osz", &payload).await;
    let cold_map = ids(&cold)[0];
    let cold_set = cold["package"]["native_set_id"].as_i64().unwrap();
    let (status, result) = change(app, staff, cold_set, true, 0, "Owned cold withdrawal").await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let source = source_snapshot(&state.db).await;
    let before = derived_snapshot(&state.db).await;
    let retained = files_snapshot(&state.maps_root).await;
    assert_eq!(
        local_distribution::file(state, cold_map, true)
            .await
            .err()
            .unwrap()
            .0,
        StatusCode::GONE
    );
    let mut tx = state.db.begin().await.unwrap();
    assert_sqlstate(
        &insert_distribution(&mut tx, package_id(&cold), Uuid::new_v4())
            .await
            .unwrap_err(),
        "P2001",
    );
    tx.rollback().await.unwrap();
    assert_eq!(source_snapshot(&state.db).await, source);
    assert_eq!(derived_snapshot(&state.db).await, before);
    assert_eq!(files_snapshot(&state.maps_root).await, retained);
}

async fn token(
    db: &PgPool,
    user: i64,
    map: i64,
    checksum: Option<&str>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO lazer_score_tokens(user_id,beatmap_id,ruleset_id,version_hash,expires_at,client_beatmap_md5) VALUES($1,$2,0,'owned-fixture',now()+interval '1 hour',$3) RETURNING id")
        .bind(user).bind(map).bind(checksum).fetch_one(db).await
}

async fn verify_token_checksum_admission(state: &AppState, user: i64, archived: &Value) {
    let maps = ids(archived);
    let (original, derived): (String, String) = sqlx::query_as("SELECT original_md5,derived_md5 FROM local_beatmap_distribution_members WHERE beatmap_id=$1")
        .bind(maps[0]).fetch_one(&state.db).await.unwrap();
    let mut tokens = Vec::new();
    for checksum in [None, Some(original.as_str()), Some(derived.as_str())] {
        let id = token(&state.db, user, maps[0], checksum).await.unwrap();
        let persisted: Option<String> =
            sqlx::query_scalar("SELECT client_beatmap_md5 FROM lazer_score_tokens WHERE id=$1")
                .bind(id)
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(persisted.as_deref(), checksum);
        let resolved: String = sqlx::query_scalar("SELECT COALESCE(t.client_beatmap_md5,b.md5) FROM lazer_score_tokens t JOIN beatmaps b ON b.id=t.beatmap_id WHERE t.id=$1")
            .bind(id).fetch_one(&state.db).await.unwrap();
        assert_eq!(
            resolved,
            checksum.unwrap_or(&original),
            "legacy NULL stays bound to the original checksum"
        );
        tokens.push((id, checksum));
    }
    let before: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(t) ORDER BY id) FROM lazer_score_tokens t")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_sqlstate(
        &token(&state.db, user, maps[1], Some(&derived))
            .await
            .unwrap_err(),
        "23514",
    );
    assert_sqlstate(
        &token(
            &state.db,
            user,
            maps[0],
            Some("ABCDEF0123456789ABCDEF0123456789"),
        )
        .await
        .unwrap_err(),
        "23514",
    );
    for (id, admitted) in tokens {
        let replacement = if admitted == Some(derived.as_str()) {
            Some(original.as_str())
        } else {
            Some(derived.as_str())
        };
        assert_sqlstate(
            &sqlx::query("UPDATE lazer_score_tokens SET client_beatmap_md5=$2 WHERE id=$1")
                .bind(id)
                .bind(replacement)
                .execute(&state.db)
                .await
                .unwrap_err(),
            "23514",
        );
        assert_sqlstate(
            &sqlx::query("UPDATE lazer_score_tokens SET beatmap_id=$2 WHERE id=$1")
                .bind(id)
                .bind(maps[1])
                .execute(&state.db)
                .await
                .unwrap_err(),
            "23514",
        );
    }
    let after: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(t) ORDER BY id) FROM lazer_score_tokens t")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(
        after, before,
        "rejected admission and rebinding never change accepted token identities"
    );
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn local_distribution_preserves_sources_complete_manifest_and_checksum_identity() {
    let fixture = MutationFixture::new("distribution", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (owner, _, _, cookie) = actor(&state, "DistributionOwner", &["beatmaps.upload"]).await;
        let (_, _, _, staff) = actor(&state, "DistributionModerator", &["beatmaps.moderate"]).await;
        let archived = verify_complete_archive(&state, &app, &cookie).await;
        verify_token_checksum_admission(&state, owner, &archived).await;
        verify_namespace_and_immutable_guards(&state, &app, &cookie).await;
        verify_standalone_and_withdrawal(&state, &app, &cookie, &staff, &archived).await;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM local_beatmap_distributions")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(
            count, 2,
            "only the complete active OSZ and standalone OSU were materialized"
        );
        let members: i64 =
            sqlx::query_scalar("SELECT count(*) FROM local_beatmap_distribution_members")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(members, 3);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn orphaned_local_distribution_preserves_archive_without_forging_identity() {
    let fixture = MutationFixture::new("distribution_orphans", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (_, _, _, cookie) = actor(&state, "DistributionOrphanOwner", &["beatmaps.upload"]).await;
        let current = difficulty("Retained orphan package", "Current");
        let retired = difficulty("Retained orphan package", "Retired");
        let original = archive(&[
            ("maps/current.osu", &current),
            ("maps/retired.osu", &retired),
            ("assets/audio.mp3", b"owned retained audio"),
            ("images/background.png", b"owned retained background"),
        ]);
        let created = upload(&app, &cookie, "orphaned.osz", &original).await;
        let package = package_id(&created);
        let current_md5 = format!("{:x}", md5::compute(&current));
        let retired_md5 = format!("{:x}", md5::compute(&retired));
        let current_item = created["items"].as_array().unwrap().iter()
            .find(|item| item["md5"] == current_md5).unwrap();
        let retired_item = created["items"].as_array().unwrap().iter()
            .find(|item| item["md5"] == retired_md5).unwrap();
        let current_id = current_item["id"].as_i64().unwrap();
        let retired_id = retired_item["id"].as_i64().unwrap();
        let set_id = created["package"]["native_set_id"].as_i64().unwrap();
        let immutable_package: Value = sqlx::query_scalar("SELECT to_jsonb(p) FROM beatmap_packages p WHERE id=$1")
            .bind(package).fetch_one(&state.db).await.unwrap();
        let deleted_row: Value = sqlx::query_scalar("SELECT to_jsonb(b) FROM beatmaps b WHERE id=$1")
            .bind(retired_id).fetch_one(&state.db).await.unwrap();
        assert_eq!(sqlx::query("DELETE FROM beatmaps WHERE id=$1")
            .bind(retired_id).execute(&state.db).await.unwrap().rows_affected(), 1);
        let before = source_snapshot(&state.db).await;
        let derived_before = derived_snapshot(&state.db).await;
        let retained = files_snapshot(&state.maps_root).await;
        assert_eq!(retained.len(), 3, "deleting a row does not rewrite or remove retained uploads");

        let mut tx = state.db.begin().await.unwrap();
        let null_archive = sqlx::query("INSERT INTO local_beatmap_distributions(package_id,id,native_set_id,format_version,kind,original_sha256,original_byte_length,archive_path,sha256,byte_length,retained_orphan_md5) SELECT id,$2,native_set_id,1,kind,sha256,byte_length,NULL,repeat('a',64),127,$3 FROM beatmap_packages WHERE id=$1")
            .bind(package).bind(Uuid::new_v4()).bind(vec![retired_md5.clone()]).execute(&mut *tx).await.unwrap_err();
        assert_sqlstate(&null_archive, "23514");
        tx.rollback().await.unwrap();
        assert_eq!(source_snapshot(&state.db).await, before);
        assert_eq!(derived_snapshot(&state.db).await, derived_before);

        for failure in ["omitted", "overlap", "duplicate", "identity"] {
            let mut orphans = match failure {
                "omitted" => Vec::new(),
                "overlap" => vec![current_md5.clone(), retired_md5.clone()],
                "duplicate" => vec![retired_md5.clone(), retired_md5.clone()],
                "identity" => vec![retired_md5.clone()],
                _ => unreachable!(),
            };
            orphans.sort_unstable();
            let distribution = Uuid::new_v4();
            let checksum = format!("{:x}", md5::compute(distribution.as_bytes()));
            let mut tx = state.db.begin().await.unwrap();
            let inserted = insert_distribution_with_orphans(&mut tx, package, distribution, &orphans).await;
            if failure == "duplicate" {
                assert_sqlstate(&inserted.unwrap_err(), "23514");
                tx.rollback().await.unwrap();
            } else {
                inserted.unwrap();
                if failure == "identity" {
                    let error = sqlx::query("INSERT INTO local_beatmap_distribution_members(distribution_id,beatmap_id,original_md5,derived_md5,native_id,native_set_id,osu_path,sha256,byte_length) SELECT $1,id,md5,$3,native_id+1,native_set_id,'distribution-'||$1::uuid::text||'/'||$3||'.osu',repeat('b',64),42 FROM beatmaps WHERE id=$2")
                        .bind(distribution).bind(current_id).bind(&checksum).execute(&mut *tx).await.unwrap_err();
                    assert_sqlstate(&error, "23514");
                    tx.rollback().await.unwrap();
                } else {
                    insert_member(&mut tx, distribution, current_id, &checksum).await.unwrap();
                    assert_sqlstate(&tx.commit().await.unwrap_err(), "23514");
                }
            }
            assert_eq!(source_snapshot(&state.db).await, before, "{failure} publishes no cursor event or source changes");
            assert_eq!(derived_snapshot(&state.db).await, derived_before, "{failure}");
            assert_eq!(files_snapshot(&state.maps_root).await, retained, "{failure}");
        }

        // A stored chart with changed identity bytes cannot be authenticated by
        // the original immutable manifest, even if its structure remains valid.
        let original_chart_path: String = sqlx::query_scalar("SELECT osu_path FROM beatmaps WHERE id=$1")
            .bind(current_id).fetch_one(&state.db).await.unwrap();
        let tampered = String::from_utf8(current.clone()).unwrap()
            .replace("BeatmapID:-1", "BeatmapID:12345").into_bytes();
        tokio::fs::write(state.maps_root.join(&original_chart_path), &tampered).await.unwrap();
        let error = local_distribution::file(&state, current_id, true).await.err().unwrap();
        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(source_snapshot(&state.db).await, before);
        assert_eq!(derived_snapshot(&state.db).await, derived_before);
        assert_eq!(files_snapshot(&state.maps_root).await.len(), retained.len(), "identity corruption leaves no staged derivative");
        tokio::fs::write(state.maps_root.join(&original_chart_path), &current).await.unwrap();
        assert_eq!(files_snapshot(&state.maps_root).await, retained);

        let distributed = local_distribution::file(&state, current_id, true).await.unwrap();
        assert_eq!(distributed.sha256, digest(&distributed.bytes));
        assert!(distributed.md5.is_none());
        let after = source_snapshot(&state.db).await;
        assert_materialization_event(&before, &after, set_id);
        let package_after: Value = sqlx::query_scalar("SELECT to_jsonb(p) FROM beatmap_packages p WHERE id=$1")
            .bind(package).fetch_one(&state.db).await.unwrap();
        assert_eq!(package_after, immutable_package, "full original two-chart manifest is immutable");
        let original_entries = entries(&original);
        let downloaded_entries = entries(&distributed.bytes);
        assert_eq!(original_entries.keys().collect::<Vec<_>>(), downloaded_entries.keys().collect::<Vec<_>>());
        assert_eq!(downloaded_entries["maps/retired.osu"], retired, "orphan bytes keep their original unregistered identity");
        assert_eq!(metadata_id(&downloaded_entries["maps/retired.osu"], "BeatmapID"), -1);
        assert_eq!(metadata_id(&downloaded_entries["maps/retired.osu"], "BeatmapSetID"), -1);
        assert_eq!(without_ids(&downloaded_entries["maps/current.osu"]), without_ids(&current));
        assert_eq!(metadata_id(&downloaded_entries["maps/current.osu"], "BeatmapID"), current_item["native_id"].as_i64().unwrap());
        assert_eq!(metadata_id(&downloaded_entries["maps/current.osu"], "BeatmapSetID"), set_id);
        for name in ["assets/audio.mp3", "images/background.png"] {
            assert_eq!(downloaded_entries[name], original_entries[name]);
        }
        let published = derived_snapshot(&state.db).await;
        assert_eq!(published["distributions"].as_array().unwrap().len(), 1);
        assert_eq!(published["distributions"][0]["retained_orphan_md5"], serde_json::json!([retired_md5]));
        assert_eq!(published["members"].as_array().unwrap().len(), 1);
        assert_eq!(published["members"][0]["beatmap_id"], current_id);
        assert_eq!(published["members"][0]["original_md5"], current_md5);
        assert_eq!(published["members"][0]["derived_md5"], format!("{:x}", md5::compute(&downloaded_entries["maps/current.osu"])));
        let orphan_aliases: i64 = sqlx::query_scalar("SELECT count(*) FROM beatmap_checksum_identities WHERE md5=$1")
            .bind(&retired_md5).fetch_one(&state.db).await.unwrap();
        assert_eq!(orphan_aliases, 0);
        let orphan_preferred: Option<String> = sqlx::query_scalar("SELECT illya_beatmap_preferred_checksum($1)")
            .bind(retired_id).fetch_one(&state.db).await.unwrap();
        assert!(orphan_preferred.is_none());
        assert!(!sqlx::query_scalar::<_, bool>("SELECT illya_beatmap_checksum_matches($1,$2)")
            .bind(retired_id).bind(&retired_md5).fetch_one(&state.db).await.unwrap());
        assert_eq!(local_distribution::file(&state, retired_id, false).await.err().unwrap().0, StatusCode::NOT_FOUND);
        let files = files_snapshot(&state.maps_root).await;
        assert_eq!(files.len(), 5, "only the current chart and complete archive get derived files");
        for (name, bytes) in retained {
            assert_eq!(files.get(&name), Some(&bytes), "original retained bytes {name}");
        }

        assert_sqlstate(&sqlx::query("DELETE FROM beatmaps WHERE id=$1")
            .bind(current_id).execute(&state.db).await.unwrap_err(), "23503");
        let reclaim = sqlx::query("INSERT INTO beatmaps(local_package_id,uploaded_by,title,artist,creator,version,mode,stars,md5,osu_path,archive_path) SELECT id,uploaded_by,'Retained orphan package','Illyaverse','Mutation fixture','Retired',0,1,$2,$3,archive_path FROM beatmap_packages WHERE id=$1")
            .bind(package).bind(&retired_md5).bind(deleted_row["osu_path"].as_str().unwrap()).execute(&state.db).await.unwrap_err();
        assert_sqlstate(&reclaim, "23514");
        assert_eq!(source_snapshot(&state.db).await, after);
        assert_eq!(derived_snapshot(&state.db).await, published);
        assert_eq!(files_snapshot(&state.maps_root).await, files);
        let repeated = local_distribution::file(&state, current_id, true).await.unwrap();
        assert_eq!(repeated.bytes, distributed.bytes);
        assert_eq!(source_snapshot(&state.db).await, after);
    }).await;
    fixture.finish(result).await;
}

async fn original_checksum_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    official_id: i64,
    checksum: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO beatmaps(official_id,official_set_id,title,artist,creator,version,mode,stars,md5,osu_path,source) VALUES($1,$1,'Concurrent checksum fixture','Fixture','Fixture','Normal',0,1,$2,'unread-fence-fixture.osu','official') RETURNING id")
        .bind(official_id).bind(checksum).fetch_one(&mut **tx).await
}

async fn assert_checksum_fence_wait(
    state: &AppState,
    application: &str,
    outer_query: &str,
    blocker: i32,
    contender: i32,
) {
    wait_for_lock(&state.db, application, outer_query).await;
    let advisory: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE pid=$1 AND locktype='advisory' AND NOT granted)")
        .bind(contender).fetch_one(&state.db).await.unwrap();
    assert!(
        advisory,
        "the contender is waiting on an advisory checksum fence"
    );
    let blocking: Vec<i32> = sqlx::query_scalar("SELECT pg_blocking_pids($1)")
        .bind(contender)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert!(
        blocking.contains(&blocker),
        "the uncommitted checksum publisher is the actual blocker"
    );
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn checksum_namespace_fences_recheck_both_concurrent_publication_orders() {
    let fixture = MutationFixture::new("distribution_fences", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let application = fixture.schema.clone();
    let result = tokio::spawn(async move {
        let (_, _, _, cookie) = actor(&state, "DistributionFenceOwner", &["beatmaps.upload"]).await;
        for (index, derived_first) in [true, false].into_iter().enumerate() {
            let raw = difficulty(&format!("Concurrent checksum order {index}"), "Normal");
            let created = upload(&app, &cookie, "fence.osu", &raw).await;
            let package = package_id(&created);
            let map = ids(&created)[0];
            let checksum = format!(
                "{:x}",
                md5::compute(format!("owned contested checksum {index}"))
            );
            let retained = files_snapshot(&state.maps_root).await;
            let original_before = source_snapshot(&state.db).await;
            let derived_before = derived_snapshot(&state.db).await;
            let mut winner = state.db.begin().await.unwrap();
            let winner_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *winner)
                .await
                .unwrap();
            let (ready, contender_pid) = tokio::sync::oneshot::channel();
            if derived_first {
                let distribution = Uuid::new_v4();
                insert_distribution(&mut winner, package, distribution)
                    .await
                    .unwrap();
                insert_member(&mut winner, distribution, map, &checksum)
                    .await
                    .unwrap();
                let db = state.db.clone();
                let pending_checksum = checksum.clone();
                let pending = tokio::spawn(async move {
                    let mut tx = db.begin().await.unwrap();
                    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                        .fetch_one(&mut *tx)
                        .await
                        .unwrap();
                    ready.send(pid).unwrap();
                    let outcome =
                        original_checksum_in_tx(&mut tx, 761_001, &pending_checksum).await;
                    if outcome.is_ok() {
                        tx.commit().await.unwrap();
                    } else {
                        tx.rollback().await.unwrap();
                    }
                    outcome
                });
                assert_checksum_fence_wait(
                    &state,
                    &application,
                    "INSERT INTO beatmaps(official_id,official_set_id,title",
                    winner_pid,
                    contender_pid.await.unwrap(),
                )
                .await;
                winner.commit().await.unwrap();
                assert_sqlstate(&pending.await.unwrap().unwrap_err(), "23505");
                let after = source_snapshot(&state.db).await;
                assert_materialization_event(
                    &original_before,
                    &after,
                    created["package"]["native_set_id"].as_i64().unwrap(),
                );
                let identity: Vec<(i64, bool)> = sqlx::query_as(
                    "SELECT beatmap_id,derived FROM beatmap_checksum_identities WHERE md5=$1",
                )
                .bind(&checksum)
                .fetch_all(&state.db)
                .await
                .unwrap();
                assert_eq!(identity, vec![(map, true)]);
                let published = derived_snapshot(&state.db).await;
                assert_eq!(published["distributions"].as_array().unwrap().len(), 1);
                assert_eq!(published["members"].as_array().unwrap().len(), 1);
            } else {
                let official = original_checksum_in_tx(&mut winner, 761_002, &checksum)
                    .await
                    .unwrap();
                let db = state.db.clone();
                let pending_checksum = checksum.clone();
                let pending = tokio::spawn(async move {
                    let mut tx = db.begin().await.unwrap();
                    let distribution = Uuid::new_v4();
                    insert_distribution(&mut tx, package, distribution)
                        .await
                        .unwrap();
                    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                        .fetch_one(&mut *tx)
                        .await
                        .unwrap();
                    ready.send(pid).unwrap();
                    let outcome =
                        insert_member(&mut tx, distribution, map, &pending_checksum).await;
                    if outcome.is_ok() {
                        tx.commit().await.unwrap();
                    } else {
                        tx.rollback().await.unwrap();
                    }
                    outcome
                });
                assert_checksum_fence_wait(
                    &state,
                    &application,
                    "INSERT INTO local_beatmap_distribution_members(distribution_id",
                    winner_pid,
                    contender_pid.await.unwrap(),
                )
                .await;
                winner.commit().await.unwrap();
                assert_sqlstate(&pending.await.unwrap().unwrap_err(), "23505");
                assert_eq!(
                    derived_snapshot(&state.db).await,
                    derived_before,
                    "losing derivative and its cursor event roll back together"
                );
                let identity: Vec<(i64, bool)> = sqlx::query_as(
                    "SELECT beatmap_id,derived FROM beatmap_checksum_identities WHERE md5=$1",
                )
                .bind(&checksum)
                .fetch_all(&state.db)
                .await
                .unwrap();
                assert_eq!(identity, vec![(official, false)]);
                let after = source_snapshot(&state.db).await;
                assert_eq!(
                    after.maps.as_array().unwrap().len(),
                    original_before.maps.as_array().unwrap().len() + 1
                );
                assert_eq!(after.packages, original_before.packages);
                assert_eq!(after.aliases, original_before.aliases);
                assert_eq!(after.audits, original_before.audits);
                assert_eq!(
                    after.updates.as_array().unwrap().len(),
                    original_before.updates.as_array().unwrap().len() + 1,
                    "only the winning original map contributes a cursor event"
                );
            }
            assert_eq!(
                files_snapshot(&state.maps_root).await,
                retained,
                "SQL-only checksum arbitration never changes retained files"
            );
        }
    })
    .await;
    fixture.finish(result).await;
}

async fn reinsert_orphan(
    tx: &mut Transaction<'_, Postgres>,
    package: Uuid,
    checksum: &str,
    path: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO beatmaps(local_package_id,uploaded_by,title,artist,creator,version,mode,stars,md5,osu_path,archive_path) SELECT id,uploaded_by,'Concurrent retained orphan','Illyaverse','Mutation fixture','Retired',0,1,$2,$3,archive_path FROM beatmap_packages WHERE id=$1 RETURNING id")
        .bind(package).bind(checksum).bind(path).fetch_one(&mut **tx).await
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn direct_sql_orphan_publication_fences_recheck_both_insertion_orders() {
    let fixture = MutationFixture::new("orphan_fences", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let application = fixture.schema.clone();
    let result = tokio::spawn(async move {
        let actual_application: String = sqlx::query_scalar("SHOW application_name")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(actual_application, application, "the lock observer must use the exact untruncated connection identity");
        let (_, _, _, cookie) = actor(&state, "DistributionOrphanFenceOwner", &["beatmaps.upload"]).await;
        for (index, publication_first) in [true, false].into_iter().enumerate() {
            let current = difficulty(&format!("Direct orphan fence {index}"), "Current");
            let retired = difficulty(&format!("Direct orphan fence {index}"), "Retired");
            let payload = archive(&[("current.osu", &current), ("retired.osu", &retired)]);
            let created = upload(&app, &cookie, "orphan-fence.osz", &payload).await;
            let package = package_id(&created);
            let current_md5 = format!("{:x}", md5::compute(&current));
            let orphan_md5 = format!("{:x}", md5::compute(&retired));
            let current_id: i64 = sqlx::query_scalar("SELECT id FROM beatmaps WHERE md5=$1")
                .bind(&current_md5).fetch_one(&state.db).await.unwrap();
            let (retired_id, orphan_path): (i64, String) = sqlx::query_as("SELECT id,osu_path FROM beatmaps WHERE md5=$1")
                .bind(&orphan_md5).fetch_one(&state.db).await.unwrap();
            sqlx::query("DELETE FROM beatmaps WHERE id=$1").bind(retired_id).execute(&state.db).await.unwrap();
            let before = source_snapshot(&state.db).await;
            let derived_before = derived_snapshot(&state.db).await;
            let retained = files_snapshot(&state.maps_root).await;
            let checksum = format!("{:x}", md5::compute(format!("owned direct orphan alias {index}")));
            let mut winner = state.db.begin().await.unwrap();
            let winner_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *winner).await.unwrap();
            let (ready, contender_pid) = tokio::sync::oneshot::channel();
            if publication_first {
                let distribution = Uuid::new_v4();
                insert_distribution_with_orphans(&mut winner, package, distribution, std::slice::from_ref(&orphan_md5)).await.unwrap();
                insert_member(&mut winner, distribution, current_id, &checksum).await.unwrap();
                sqlx::query("SET CONSTRAINTS local_distribution_complete,local_distribution_member_complete IMMEDIATE")
                    .execute(&mut *winner).await.unwrap();
                let db = state.db.clone();
                let pending_md5 = orphan_md5.clone();
                let pending_path = orphan_path.clone();
                let pending = tokio::spawn(async move {
                    let mut tx = db.begin().await.unwrap();
                    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                        .fetch_one(&mut *tx).await.unwrap();
                    ready.send(pid).unwrap();
                    let outcome = reinsert_orphan(&mut tx, package, &pending_md5, &pending_path).await;
                    if outcome.is_ok() { tx.commit().await.unwrap(); } else { tx.rollback().await.unwrap(); }
                    outcome
                });
                assert_checksum_fence_wait(&state, &application, "INSERT INTO beatmaps(local_package_id,uploaded_by,title", winner_pid, contender_pid.await.unwrap()).await;
                winner.commit().await.unwrap();
                assert_sqlstate(&pending.await.unwrap().unwrap_err(), "23514");
                assert_materialization_event(&before, &source_snapshot(&state.db).await, created["package"]["native_set_id"].as_i64().unwrap());
                let published = derived_snapshot(&state.db).await;
                assert_eq!(published["distributions"].as_array().unwrap().len(), 1);
                assert_eq!(published["members"].as_array().unwrap().len(), 1);
                let orphan_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM beatmap_checksum_identities WHERE md5=$1")
                    .bind(&orphan_md5).fetch_one(&state.db).await.unwrap();
                assert_eq!(orphan_rows, 0, "publication wins without registering a deleted chart");
            } else {
                let restored = reinsert_orphan(&mut winner, package, &orphan_md5, &orphan_path).await.unwrap();
                let db = state.db.clone();
                let pending_md5 = orphan_md5.clone();
                let pending_checksum = checksum.clone();
                let pending = tokio::spawn(async move {
                    let mut tx = db.begin().await.unwrap();
                    let distribution = Uuid::new_v4();
                    insert_distribution_with_orphans(&mut tx, package, distribution, &[pending_md5]).await.unwrap();
                    insert_member(&mut tx, distribution, current_id, &pending_checksum).await.unwrap();
                    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                        .fetch_one(&mut *tx).await.unwrap();
                    ready.send(pid).unwrap();
                    tx.commit().await
                });
                assert_checksum_fence_wait(&state, &application, "COMMIT", winner_pid, contender_pid.await.unwrap()).await;
                winner.commit().await.unwrap();
                assert_sqlstate(&pending.await.unwrap().unwrap_err(), "23514");
                assert_eq!(derived_snapshot(&state.db).await, derived_before, "failed full-manifest validation rolls back distribution, aliases and cursor together");
                let identity: Vec<(i64, bool)> = sqlx::query_as("SELECT beatmap_id,derived FROM beatmap_checksum_identities WHERE md5=$1")
                    .bind(&orphan_md5).fetch_all(&state.db).await.unwrap();
                assert_eq!(identity, vec![(restored, false)]);
                let alias_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM beatmap_checksum_identities WHERE md5=$1")
                    .bind(&checksum).fetch_one(&state.db).await.unwrap();
                assert_eq!(alias_rows, 0);
                let after = source_snapshot(&state.db).await;
                assert_eq!(after.maps.as_array().unwrap().len(), before.maps.as_array().unwrap().len() + 1);
                assert_eq!(after.packages, before.packages);
                assert_eq!(after.aliases, before.aliases);
                assert_eq!(after.audits, before.audits);
                assert_eq!(after.updates.as_array().unwrap().len(), before.updates.as_array().unwrap().len() + 1, "only the winning inserted map contributes an update");
            }
            assert_eq!(files_snapshot(&state.maps_root).await, retained, "SQL-only orphan fencing preserves every retained byte");
        }
    }).await;
    fixture.finish(result).await;
}
