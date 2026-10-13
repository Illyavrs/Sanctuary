use super::*;
use crate::{
    Router,
    maps::map_mutation_tests::{
        MutationFixture, actor, reject_audits_at_commit, wait_for_lock, wait_until_expired,
    },
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use image::{Rgba, RgbaImage};
use serde_json::{Value, json};
use std::time::Duration;
use tower::ServiceExt;

fn frame_image(width: u32, height: u32) -> DynamicImage {
    let mut image = RgbaImage::new(width, height);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        if x < width / 8 || x >= width * 7 / 8 || y < height / 8 || y >= height * 7 / 8 {
            *pixel = Rgba([120, 80, 240, 180]);
        }
    }
    DynamicImage::ImageRgba8(image)
}

fn encode(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, format).unwrap();
    bytes.into_inner()
}

fn png_chunk(kind: &[u8; 4], bytes: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::new();
    chunk.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(bytes);
    chunk.extend_from_slice(&crc32fast::hash(&chunk[4..]).to_be_bytes());
    chunk
}

#[test]
fn extended_webp_rejects_oversized_inner_frames_before_pixel_decode() {
    fn container(canvas: u32, frame: u16, lossless: bool) -> Vec<u8> {
        let mut body = b"WEBP".to_vec();
        body.extend_from_slice(b"VP8X");
        body.extend_from_slice(&10u32.to_le_bytes());
        body.extend_from_slice(&[0, 0, 0, 0]);
        body.extend_from_slice(&(canvas - 1).to_le_bytes()[..3]);
        body.extend_from_slice(&(canvas - 1).to_le_bytes()[..3]);
        if lossless {
            body.extend_from_slice(b"VP8L");
            body.extend_from_slice(&5u32.to_le_bytes());
            body.push(0x2f);
            let size = u32::from(frame - 1);
            body.extend_from_slice(&(size | (size << 14)).to_le_bytes());
            body.push(0);
        } else {
            body.extend_from_slice(b"VP8 ");
            body.extend_from_slice(&10u32.to_le_bytes());
            body.extend_from_slice(&[0, 0, 0, 0x9d, 0x01, 0x2a]);
            body.extend_from_slice(&frame.to_le_bytes());
            body.extend_from_slice(&frame.to_le_bytes());
        }
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&body);
        bytes
    }
    for lossless in [false, true] {
        let oversized = container(128, 16383, lossless);
        // This is only a frame header: rejection must come from dimensions,
        // before the native decoder allocates pixels and hits missing data.
        assert!(canonical_png(oversized).unwrap_err().2.contains("128–1024"));
        let mismatched = container(128, 512, lossless);
        assert!(
            canonical_png(mismatched)
                .unwrap_err()
                .2
                .contains("must match")
        );
        assert!(webp_preflight(&container(128, 128, lossless)).is_ok());
    }
    let mut bad_length = container(128, 128, false);
    bad_length[34..38].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(webp_preflight(&bad_length).is_err());
    let mut trailing = container(128, 128, false);
    trailing.extend_from_slice(&[0, 0]);
    assert!(webp_preflight(&trailing).is_err());
}

#[test]
fn frame_png_and_webp_keep_rgba_and_clear_avatar_opening() {
    let input = frame_image(128, 128);
    for format in [ImageFormat::Png, ImageFormat::WebP] {
        let output = canonical_png(encode(&input, format)).unwrap();
        assert_eq!(image::guess_format(&output).unwrap(), ImageFormat::Png);
        let image = image::load_from_memory(&output).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), (128, 128));
        assert_eq!(image.get_pixel(64, 64).0, [0, 0, 0, 0]);
        assert_eq!(image.get_pixel(0, 0).0, [120, 80, 240, 180]);
    }
    assert!(canonical_png(encode(&frame_image(1024, 1024), ImageFormat::Png)).is_ok());
}

#[test]
fn frame_rejects_unsupported_opaque_empty_animated_and_bounded_inputs() {
    for bytes in [
        Vec::new(),
        vec![0; MAX_BYTES + 1],
        b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec(),
        encode(&frame_image(128, 128).to_rgb8().into(), ImageFormat::Jpeg),
        encode(
            &DynamicImage::ImageRgba8(RgbaImage::from_pixel(128, 128, Rgba([20, 30, 40, 255]))),
            ImageFormat::Png,
        ),
        encode(
            &DynamicImage::ImageRgba8(RgbaImage::new(128, 128)),
            ImageFormat::Png,
        ),
    ] {
        assert!(canonical_png(bytes).is_err());
    }
    for (width, height) in [(127, 127), (128, 129), (1025, 1025)] {
        assert!(canonical_png(encode(&frame_image(width, height), ImageFormat::Png)).is_err());
    }
    let png = encode(&frame_image(128, 128), ImageFormat::Png);
    // A valid one-frame APNG is still animated artwork and must be rejected.
    let mut apng = png[..33].to_vec();
    apng.extend_from_slice(&png_chunk(b"acTL", &[0, 0, 0, 1, 0, 0, 0, 0]));
    let mut control = Vec::new();
    for value in [0u32, 128, 128, 0, 0] {
        control.extend_from_slice(&value.to_be_bytes());
    }
    control.extend_from_slice(&[0, 1, 0, 10, 0, 0]);
    apng.extend_from_slice(&png_chunk(b"fcTL", &control));
    apng.extend_from_slice(&png[33..]);
    let decoder = image::codecs::png::PngDecoder::new(Cursor::new(&apng)).unwrap();
    assert!(decoder.is_apng().unwrap());
    assert!(canonical_png(apng).unwrap_err().2.contains("Animated"));

    // Wrap the existing lossless bitstream in a valid animated WebP container.
    let webp = encode(&frame_image(128, 128), ImageFormat::WebP);
    fn riff_chunk(kind: &[u8; 4], bytes: &[u8]) -> Vec<u8> {
        let mut chunk = kind.to_vec();
        chunk.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        chunk.extend_from_slice(bytes);
        if bytes.len() % 2 == 1 {
            chunk.push(0);
        }
        chunk
    }
    let mut body = b"WEBP".to_vec();
    body.extend_from_slice(&riff_chunk(b"VP8X", &[0x12, 0, 0, 0, 127, 0, 0, 127, 0, 0]));
    body.extend_from_slice(&riff_chunk(b"ANIM", &[0, 0, 0, 0, 0, 0]));
    let mut frame = vec![0, 0, 0, 0, 0, 0, 127, 0, 0, 127, 0, 0, 100, 0, 0, 0];
    frame.extend_from_slice(&webp[12..]);
    body.extend_from_slice(&riff_chunk(b"ANMF", &frame));
    let mut animated_webp = b"RIFF".to_vec();
    animated_webp.extend_from_slice(&(body.len() as u32).to_le_bytes());
    animated_webp.extend_from_slice(&body);
    let decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(&animated_webp)).unwrap();
    assert!(decoder.has_animation());
    assert!(
        canonical_png(animated_webp)
            .unwrap_err()
            .2
            .contains("Animated")
    );

    // A forged large IHDR is rejected before any pixel buffer allocation.
    let mut huge = png.clone();
    huge[16..20].copy_from_slice(&1_000_000u32.to_be_bytes());
    huge[20..24].copy_from_slice(&1_000_000u32.to_be_bytes());
    let crc = crc32fast::hash(&huge[12..29]);
    huge[29..33].copy_from_slice(&crc.to_be_bytes());
    assert!(canonical_png(huge).unwrap_err().2.contains("too large"));
    assert!(canonical_png(png[..png.len() / 2].to_vec()).is_err());
}

async fn json_request(
    app: &Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::ORIGIN, "http://localhost:3000")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(if method == "GET" {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
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

fn fields(revision: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = format!("--frame-fixture\r\nContent-Disposition: form-data; name=\"revision\"\r\n\r\n{revision}\r\n--frame-fixture\r\nContent-Disposition: form-data; name=\"file\"; filename=\"../../misleading.svg\"\r\nContent-Type: text/plain\r\n\r\n").into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n--frame-fixture--\r\n");
    body
}

async fn upload(
    app: &Router,
    id: i64,
    revision: i64,
    cookie: Option<&str>,
    bytes: Vec<u8>,
) -> (StatusCode, Value) {
    crate::maps::map_mutation_tests::request(
        app,
        &format!("/api/v1/admin/rewards/{id}/image"),
        cookie,
        false,
        "multipart/form-data; boundary=frame-fixture",
        fields(&revision.to_string(), &bytes),
    )
    .await
}

fn draft_body() -> Value {
    json!({"name":"Uploaded frame fixture","description":"Test artwork","kind":"avatar_frame","price":20,"color":null,"style":"image","active":false})
}

async fn draft(app: &Router, cookie: &str) -> Value {
    let (status, item) = json_request(
        app,
        "POST",
        "/api/v1/admin/rewards",
        Some(cookie),
        draft_body(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{item}");
    assert_eq!(item["image_url"], Value::Null);
    item
}

fn edit_body(item: &Value, active: bool) -> Value {
    json!({"name":item["name"],"description":item["description"],"kind":item["kind"],"price":item["price"],"color":item["color"],"style":item["style"],"active":active,"revision":item["revision"]})
}

async fn file_count(root: &std::path::Path) -> usize {
    let mut count = 0;
    for directory in ["reward-frames", ".illya-media-staging"] {
        if let Ok(mut entries) = tokio::fs::read_dir(root.join(directory)).await {
            while entries.next_entry().await.unwrap().is_some() {
                count += 1;
            }
        }
    }
    count
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn uploaded_frames_publish_purchase_equip_delist_and_retain_immutable_media() {
    let fixture = MutationFixture::new("frame_lifecycle", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (_, _, _, manager) = actor(&state, "ImageFrameManager", &["rewards.manage"]).await;
        let (owner, _, _, player) = actor(&state, "ImageFrameOwner", &[]).await;
        let bytes = encode(&frame_image(128, 128), ImageFormat::WebP);
        let item = draft(&app, &manager).await;
        let id = item["id"].as_i64().unwrap();
        let unavailable = json_request(
            &app,
            "PATCH",
            &format!("/api/v1/admin/rewards/{id}"),
            Some(&manager),
            edit_body(&item, true),
        )
        .await;
        assert_eq!(unavailable.0, StatusCode::CONFLICT, "{}", unavailable.1);
        assert_eq!(unavailable.1["error"]["code"], "reward_image_required");
        let (_, catalog) = json_request(&app, "GET", "/api/v1/rewards", None, Value::Null).await;
        assert!(catalog["items"].as_array().unwrap().is_empty());
        let (status, uploaded) = upload(&app, id, 1, Some(&manager), bytes.clone()).await;
        assert_eq!(status, StatusCode::OK, "{uploaded}");
        assert_eq!(uploaded["revision"], 2);
        assert_eq!(uploaded["active"], false);
        let first_url = uploaded["image_url"].as_str().unwrap().to_owned();
        assert!(first_url.starts_with("/media/reward-frames/"));
        assert!(first_url.ends_with(".png"));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&first_url)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
        let media = to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(
            image::load_from_memory(&media)
                .unwrap()
                .to_rgba8()
                .get_pixel(64, 64)[3],
            0
        );
        let published = json_request(
            &app,
            "PATCH",
            &format!("/api/v1/admin/rewards/{id}"),
            Some(&manager),
            edit_body(&uploaded, true),
        )
        .await;
        assert_eq!(published.0, StatusCode::OK, "{}", published.1);
        sqlx::query("INSERT INTO reward_wallets(user_id,balance) VALUES($1,100)")
            .bind(owner)
            .execute(&state.db)
            .await
            .unwrap();
        let purchased = json_request(
            &app,
            "POST",
            "/api/v1/me/rewards/purchases",
            Some(&player),
            json!({"reward_id":id,"revision":3}),
        )
        .await;
        assert_eq!(purchased.0, StatusCode::OK, "{}", purchased.1);
        let equipped = json_request(
            &app,
            "PATCH",
            "/api/v1/me/rewards/equipment",
            Some(&player),
            json!({"title_id":null,"avatar_frame_id":id}),
        )
        .await;
        assert_eq!(equipped.0, StatusCode::OK, "{}", equipped.1);
        assert_eq!(
            equipped.1["state"]["equipment"]["avatar_frame"]["image_url"],
            first_url
        );
        let hidden = json_request(
            &app,
            "PATCH",
            &format!("/api/v1/admin/rewards/{id}"),
            Some(&manager),
            edit_body(&published.1, false),
        )
        .await;
        assert_eq!(hidden.0, StatusCode::OK, "{}", hidden.1);
        let profile = rewards::profile_equipment(&state, owner).await.unwrap();
        assert_eq!(profile["avatar_frame"]["image_url"], first_url);
        assert_eq!(profile["avatar_frame"]["active"], false);
        let replaced = upload(&app, id, 4, Some(&manager), bytes).await;
        assert_eq!(replaced.0, StatusCode::OK, "{}", replaced.1);
        assert_ne!(replaced.1["image_url"], first_url);
        assert_eq!(
            file_count(&state.media_root).await,
            2,
            "old previews retain immutable files"
        );
        assert!(
            tokio::fs::try_exists(
                state
                    .media_root
                    .join(first_url.trim_start_matches("/media/"))
            )
            .await
            .unwrap()
        );
        let unchanged = json_request(
            &app,
            "PATCH",
            &format!("/api/v1/admin/rewards/{id}"),
            Some(&manager),
            edit_body(&replaced.1, false),
        )
        .await;
        assert_eq!(unchanged.0, StatusCode::OK, "{}", unchanged.1);
        assert_eq!(unchanged.1["image_url"], replaced.1["image_url"]);
        let mut geometric = edit_body(&unchanged.1, false);
        geometric["style"] = json!("corners");
        let switched = json_request(
            &app,
            "PATCH",
            &format!("/api/v1/admin/rewards/{id}"),
            Some(&manager),
            geometric,
        )
        .await;
        assert_eq!(switched.0, StatusCode::OK, "{}", switched.1);
        assert_eq!(switched.1["image_url"], Value::Null);
        assert_eq!(file_count(&state.media_root).await, 2);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn frame_upload_rejects_permission_input_and_revision_races_without_orphans() {
    let fixture = MutationFixture::new("frame_boundaries", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (_, _, _, manager) = actor(&state, "UploadBoundaryManager", &["rewards.manage"]).await;
        let (owner, _, _, player) = actor(&state, "UploadBoundaryPlayer", &[]).await;
        let item = draft(&app, &manager).await;
        let id = item["id"].as_i64().unwrap();
        let bytes = encode(&frame_image(128, 128), ImageFormat::Png);
        sqlx::query("INSERT INTO reward_inventory(user_id,reward_id,paid) VALUES($1,$2,20)")
            .bind(owner)
            .bind(id)
            .execute(&state.db)
            .await
            .unwrap();
        let missing_asset = json_request(
            &app,
            "PATCH",
            "/api/v1/me/rewards/equipment",
            Some(&player),
            json!({"title_id":null,"avatar_frame_id":id}),
        )
        .await;
        assert_eq!(missing_asset.0, StatusCode::CONFLICT, "{}", missing_asset.1);
        assert_eq!(missing_asset.1["error"]["code"], "reward_image_required");
        let equipment: i64 =
            sqlx::query_scalar("SELECT count(*) FROM reward_equipment WHERE user_id=$1")
                .bind(owner)
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(equipment, 0);
        for invalid_url in [
            "https://example.invalid/frame.png",
            "/media/reward-frames/../../secret.png",
            "/media/avatar/123.png",
        ] {
            assert!(
                sqlx::query("UPDATE reward_catalog SET image_url=$2 WHERE id=$1")
                    .bind(id)
                    .bind(invalid_url)
                    .execute(&state.db)
                    .await
                    .is_err()
            );
        }
        assert!(
            sqlx::query("UPDATE reward_catalog SET style=NULL WHERE id=$1")
                .bind(id)
                .execute(&state.db)
                .await
                .is_err()
        );
        assert_eq!(
            upload(&app, id, 1, None, bytes.clone()).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            upload(&app, id, 1, Some(&player), bytes.clone()).await.0,
            StatusCode::FORBIDDEN
        );
        for invalid in [
            b"<svg/>".to_vec(),
            vec![0; MAX_BYTES + 1],
            encode(&frame_image(127, 127), ImageFormat::Png),
        ] {
            assert_eq!(
                upload(&app, id, 1, Some(&manager), invalid).await.0,
                StatusCode::BAD_REQUEST
            );
        }
        let mut remote = draft_body();
        remote["image_url"] = json!("https://example.invalid/frame.png");
        assert_eq!(
            json_request(
                &app,
                "POST",
                "/api/v1/admin/rewards",
                Some(&manager),
                remote
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let missing_revision = fields("not-an-id", &bytes);
        assert_eq!(
            crate::maps::map_mutation_tests::request(
                &app,
                &format!("/api/v1/admin/rewards/{id}/image"),
                Some(&manager),
                false,
                "multipart/form-data; boundary=frame-fixture",
                missing_revision
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(file_count(&state.media_root).await, 0);
        let (first, second) = tokio::join!(
            upload(&app, id, 1, Some(&manager), bytes.clone()),
            upload(&app, id, 1, Some(&manager), bytes.clone())
        );
        let success = if first.0 == StatusCode::OK {
            &first
        } else {
            &second
        };
        let stale = if first.0 == StatusCode::OK {
            &second
        } else {
            &first
        };
        assert_eq!(success.0, StatusCode::OK, "{first:?}/{second:?}");
        assert_eq!(stale.0, StatusCode::CONFLICT, "{stale:?}");
        assert_eq!(stale.1["error"]["code"], "reward_conflict");
        assert_eq!(file_count(&state.media_root).await, 1);
        let audits: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE action='rewards.upload_image'",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(audits, 1);
        let mut title = draft_body();
        title["kind"] = json!("title");
        title["style"] = Value::Null;
        let (_, title) =
            json_request(&app, "POST", "/api/v1/admin/rewards", Some(&manager), title).await;
        assert_eq!(
            upload(
                &app,
                title["id"].as_i64().unwrap(),
                1,
                Some(&manager),
                bytes
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(file_count(&state.media_root).await, 1);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn frame_upload_rechecks_queued_authority_and_expiry_and_cleans_files() {
    let fixture = MutationFixture::new("frame_authority", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let application = fixture.schema.clone();
    let result = tokio::spawn(async move {
        let (_, _, _, manager) = actor(&state, "UploadAuthoritySeeder", &["rewards.manage"]).await;
        let item = draft(&app, &manager).await; let id = item["id"].as_i64().unwrap();
        for change in ["permission", "role", "revoke", "restrict", "expire"] {
            let (user, role, sid, cookie) = actor(&state, &format!("FrameAuthority{change}"), &["rewards.manage"]).await;
            let mut blocker = state.db.begin().await.unwrap();
            sqlx::query("LOCK TABLE roles,user_roles IN SHARE ROW EXCLUSIVE MODE").execute(&mut *blocker).await.unwrap();
            let queued_app = app.clone(); let bytes = encode(&frame_image(128,128),ImageFormat::Png);
            let pending = tokio::spawn(async move { upload(&queued_app,id,1,Some(&cookie),bytes).await });
            wait_for_lock(&state.db,&application,"LOCK TABLE roles,user_roles IN SHARE MODE").await;
            match change {
                "permission" => { sqlx::query("UPDATE roles SET permissions='{}' WHERE id=$1").bind(role).execute(&mut *blocker).await.unwrap(); }
                "role" => { sqlx::query("DELETE FROM user_roles WHERE user_id=$1 AND role_id=$2").bind(user).bind(role).execute(&mut *blocker).await.unwrap(); }
                "revoke" => { sqlx::query("DELETE FROM sessions WHERE id=$1").bind(sid).execute(&mut *blocker).await.unwrap(); }
                "restrict" => { sqlx::query("UPDATE users SET is_restricted=true WHERE id=$1").bind(user).execute(&mut *blocker).await.unwrap(); }
                _ => { sqlx::query("UPDATE sessions SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(sid).execute(&mut *blocker).await.unwrap(); }
            }
            blocker.commit().await.unwrap();
            let response = tokio::time::timeout(Duration::from_secs(5),pending).await.unwrap().unwrap();
            assert_eq!(response.0,if change=="permission"||change=="role"{StatusCode::FORBIDDEN}else{StatusCode::UNAUTHORIZED},"{change}: {}",response.1);
            assert_eq!(file_count(&state.media_root).await,0);
        }
        // Authorization was valid when acquired, then expires during catalog wait.
        let (_,_,sid,cookie)=actor(&state,"FrameResourceExpiry",&["rewards.manage"]).await;
        let expiry: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("UPDATE sessions SET expires_at=clock_timestamp()+interval '2 seconds' WHERE id=$1 RETURNING expires_at").bind(sid).fetch_one(&state.db).await.unwrap();
        let mut blocker=state.db.begin().await.unwrap();
        sqlx::query("SELECT id FROM reward_catalog WHERE id=$1 FOR UPDATE").bind(id).fetch_one(&mut *blocker).await.unwrap();
        let queued_app=app.clone(); let bytes=encode(&frame_image(128,128),ImageFormat::Png);
        let pending=tokio::spawn(async move{upload(&queued_app,id,1,Some(&cookie),bytes).await});
        wait_for_lock(&state.db,&application,"SELECT revision,kind FROM reward_catalog").await;
        wait_until_expired(&state.db,expiry).await;
        blocker.commit().await.unwrap();
        let response=tokio::time::timeout(Duration::from_secs(5),pending).await.unwrap().unwrap();
        assert_eq!(response.0,StatusCode::UNAUTHORIZED,"{}",response.1);
        assert_eq!(file_count(&state.media_root).await,0);
        let stored: (i64,Option<String>)=sqlx::query_as("SELECT revision,image_url FROM reward_catalog WHERE id=$1").bind(id).fetch_one(&state.db).await.unwrap();
        assert_eq!(stored,(1,None));
    }).await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn frame_upload_audit_commit_failure_removes_only_new_owned_media() {
    let fixture = MutationFixture::new("frame_rollback", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (_, _, _, manager) = actor(&state, "FrameRollbackManager", &["rewards.manage"]).await;
        let item = draft(&app, &manager).await;
        let id = item["id"].as_i64().unwrap();
        let bytes = encode(&frame_image(128, 128), ImageFormat::Png);
        let (_, original) = upload(&app, id, 1, Some(&manager), bytes.clone()).await;
        let original_url = original["image_url"].as_str().unwrap().to_owned();
        reject_audits_at_commit(&state.db).await;
        let failed = upload(&app, id, 2, Some(&manager), bytes).await;
        assert_eq!(failed.0, StatusCode::INTERNAL_SERVER_ERROR, "{}", failed.1);
        let stored: (i64, String) =
            sqlx::query_as("SELECT revision,image_url FROM reward_catalog WHERE id=$1")
                .bind(id)
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(stored, (2, original_url.clone()));
        assert_eq!(file_count(&state.media_root).await, 1);
        assert!(
            tokio::fs::try_exists(
                state
                    .media_root
                    .join(original_url.trim_start_matches("/media/"))
            )
            .await
            .unwrap()
        );
    })
    .await;
    fixture.finish(result).await;
}
