//! Bell delivery and source publication use only an owned disposable PG17 schema.
use crate::{
    AppState, Router,
    maps::map_mutation_tests::{
        MutationFixture, actor, reject_audits_at_commit, wait_for_lock, wait_until_expired,
    },
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::{collections::BTreeSet, time::Duration};
use tower::ServiceExt;
use uuid::Uuid;

const BELL: &str = "/api/v1/me/notifications";
const READ: &str = "/api/v1/me/notifications/read";
const ANNOUNCEMENTS: &str = "/api/v1/admin/announcements";
const RANK_REQUESTS: &str = "/api/v1/beatmap-rank-requests";
const RANK_REVIEW: &str = "/api/v1/admin/beatmap-rank-requests";

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    value: Value,
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
                    Body::from(value.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    if status.is_success() && path.starts_with(BELL) {
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "private, no-store"
        );
    }
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn list(app: &Router, cookie: &str) -> Value {
    let (status, value) = request(app, "GET", BELL, Some(cookie), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}

async fn read(app: &Router, cookie: &str, input: Value) -> Value {
    let (status, value) = request(app, "POST", READ, Some(cookie), input).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}

async fn another_session(state: &AppState, user: i64) -> String {
    let sid = Uuid::new_v4();
    let expiry = chrono::Utc::now() + chrono::Duration::hours(1);
    sqlx::query("INSERT INTO sessions(id,user_id,expires_at) VALUES($1,$2,$3)")
        .bind(sid)
        .bind(user)
        .bind(expiry)
        .execute(&state.db)
        .await
        .unwrap();
    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &json!({"sub":user,"sid":sid,"exp":expiry.timestamp()}),
        &jsonwebtoken::EncodingKey::from_secret(state.secret.as_bytes()),
    )
    .unwrap();
    format!("illya_session={token}")
}

// Only this owned fixture knows its signing secret. Correct signatures with
// invalid scopes/deadlines exercise claim validation independently of parsing.
fn scoped_fixture_snapshot(
    state: &AppState,
    user: i64,
    sid: Uuid,
    rows: &[(i64, Uuid)],
    purpose: &str,
    expiry: i64,
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"Illyaverse notification read snapshot v1\0");
    hash.update(state.secret.as_bytes());
    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &json!({"purpose":purpose,"sub":user,"sid":sid,"rows":rows,"exp":expiry}),
        &jsonwebtoken::EncodingKey::from_secret(&hash.finalize()),
    )
    .unwrap();
    format!("notification-snapshot.{token}")
}

async fn identities(db: &PgPool, ids: &[i64]) -> Vec<(i64, Uuid)> {
    sqlx::query_as("SELECT id,identity FROM user_notifications WHERE id=ANY($1) ORDER BY id")
        .bind(ids)
        .fetch_all(db)
        .await
        .unwrap()
}

async fn rows(db: &PgPool, tables: &[(&str, &str)]) -> Value {
    let mut result = serde_json::Map::new();
    for (table, order) in tables {
        let value = sqlx::query_scalar::<_, Value>(&format!(
            "SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY {order}),'[]'::jsonb) FROM {table} r"
        ))
        .fetch_one(db)
        .await
        .unwrap();
        result.insert((*table).into(), value);
    }
    Value::Object(result)
}

async fn chat_snapshot(db: &PgPool) -> Value {
    rows(
        db,
        &[
            ("protocol_private_messages", "id"),
            ("chat_messages", "id"),
            ("lazer_channel_members", "user_id,channel_id"),
            ("pm_read_receipts", "reader_id,message_id"),
        ],
    )
    .await
}

async fn source_snapshot(db: &PgPool) -> Value {
    rows(
        db,
        &[
            ("announcements", "id"),
            ("beatmap_rank_requests", "id"),
            ("beatmap_status_overrides", "native_id"),
            ("beatmaps", "id"),
            ("lazer_beatmap_updates", "id"),
            ("user_notifications", "id"),
            ("audit_events", "id"),
        ],
    )
    .await
}

async fn wait_for_both_publishers(db: &PgPool, application: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity WHERE application_name=$1 \
                 AND wait_event_type='Lock' AND query LIKE '%SELECT revision,status FROM announcements%'",
            )
            .bind(application)
            .fetch_one(db)
            .await
            .unwrap();
            if waiting == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both publishers must hold their actor fences before fan-out");
}

// Route tests seed valid source-bound notices directly. Lifecycle tests below
// emit them through the actual publication and rank-review HTTP transactions.
async fn published_source(db: &PgPool, user: i64, title: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO announcements(title,body,status,created_by,updated_by,published_at) \
         VALUES($1,'Source body is not bell content','published',$2,$2,clock_timestamp()) RETURNING id",
    )
    .bind(title)
    .bind(user)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn seed(db: &PgPool, user: i64, tag: &str, count: i32) -> Vec<i64> {
    let source = published_source(db, user, tag).await;
    let mut ids: Vec<i64> = sqlx::query_scalar(
        "INSERT INTO user_notifications(user_id,event_key,kind,announcement_id,publication_at,title) \
         SELECT $1,$3 || ':' || i,'announcement_published',a.id,a.published_at,a.title \
         FROM announcements a CROSS JOIN generate_series(1,$4::int) i WHERE a.id=$2 RETURNING id",
    )
    .bind(user)
    .bind(source)
    .bind(tag)
    .bind(count)
    .fetch_all(db)
    .await
    .unwrap();
    ids.sort_unstable();
    ids
}

async fn create_announcement(app: &Router, cookie: &str, title: &str) -> Value {
    let (status, value) = request(
        app,
        "POST",
        ANNOUNCEMENTS,
        Some(cookie),
        json!({"title":title,"body":"Publication body"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    value
}

async fn update_announcement(
    app: &Router,
    cookie: &str,
    item: &Value,
    status: &str,
) -> (StatusCode, Value) {
    request(
        app,
        "PATCH",
        &format!("{ANNOUNCEMENTS}/{}", item["id"]),
        Some(cookie),
        json!({"revision":item["revision"],"title":"Updated publication", "body":"Updated body","status":status}),
    )
    .await
}

async fn local_request(state: &AppState, app: &Router, user: i64, cookie: &str) -> Value {
    let package = Uuid::new_v4();
    let checksum = format!("{:x}", md5::compute(package.as_bytes()));
    let archive = format!("notification-fixture-{package}/package.osz");
    let mut tx = state.db.begin().await.unwrap();
    let set: i64 = sqlx::query_scalar(
        "INSERT INTO beatmap_packages(id,uploaded_by,kind,member_md5,archive_path,legacy) \
         VALUES($1,$2,'osz',$3,$4,true) RETURNING native_set_id",
    )
    .bind(package)
    .bind(user)
    .bind(vec![checksum.clone()])
    .bind(&archive)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO beatmaps(local_package_id,uploaded_by,title,artist,creator,version,mode,status,stars,md5,osu_path,archive_path) \
         VALUES($1,$2,'Notification request','Fixture Artist','Fixture Creator','Easy',0,'graveyard',2,$3,$4,$5)",
    )
    .bind(package)
    .bind(user)
    .bind(checksum)
    .bind(format!("notification-fixture-{package}/map.osu"))
    .bind(archive)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (status, value) = request(
        app,
        "POST",
        RANK_REQUESTS,
        Some(cookie),
        json!({"native_set_id":set,"reason":"Please review this set"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["status"], "pending");
    value
}

async fn decide(app: &Router, cookie: &str, item: &Value, decision: &str) -> (StatusCode, Value) {
    let mut input = json!({
        "expected_revision":item["revision"],"decision":decision,"reason":"Review complete"
    });
    if decision == "approved" {
        let difficulty = &item["difficulties"][0];
        input["difficulty_ids"] = json!([difficulty["native_id"]]);
        input["difficulty_revisions"] = json!([{
            "native_id":difficulty["native_id"],
            "expected_revision":difficulty["revision"],
            "expected_status_revision":difficulty["status_revision"],
        }]);
    }
    request(
        app,
        "PATCH",
        &format!("{RANK_REVIEW}/{}", item["id"]),
        Some(cookie),
        input,
    )
    .await
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn notification_http_ownership_validation_and_chat_isolation() {
    let fixture = MutationFixture::new("bell_owner", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let outcome = tokio::spawn(async move {
        let (first, _, sid, cookie) = actor(&state, "BellOwner", &[]).await;
        let (other, _, _, foreign) = actor(&state, "BellOther", &["*"]).await;
        let own = seed(&state.db, first, "Owner publication", 2).await;
        let own_rows = identities(&state.db, &own).await;
        let other_ids = seed(&state.db, other, "Private foreign publication", 1).await;
        sqlx::query("INSERT INTO protocol_private_messages(sender_id,recipient_id,content) VALUES($1,$2,'Private PM must remain unread')")
            .bind(other).bind(first).execute(&state.db).await.unwrap();
        let chat_before = chat_snapshot(&state.db).await;
        let notices_before = rows(&state.db, &[("user_notifications", "id")]).await;

        assert_eq!(request(&app, "GET", BELL, None, Value::Null).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(request(&app, "POST", READ, None, json!({"ids":own})).await.0, StatusCode::UNAUTHORIZED);
        let page = list(&app, &cookie).await;
        assert_eq!(page["total"], 2);
        assert_eq!(page["unread_count"], 2);
        assert!(!page["snapshot_id"].as_str().unwrap().is_empty());
        let expected = ["id", "kind", "title", "message", "href", "created_at", "read_at"]
            .into_iter().collect::<BTreeSet<_>>();
        for item in page["items"].as_array().unwrap() {
            let actual = item.as_object().unwrap().keys().map(String::as_str).collect::<BTreeSet<_>>();
            assert_eq!(actual, expected, "only the public notice DTO is delivered");
            assert_eq!(item["title"], "Owner publication");
            assert!(item["message"].is_null());
            assert!(item["read_at"].is_null());
        }
        assert!(!page.to_string().contains("Private foreign"));
        assert!(!page.to_string().contains("Private PM"));
        assert_eq!(list(&app, &foreign).await["total"], 1, "wildcard permissions never grant another inbox");
        assert_eq!(rows(&state.db, &[("user_notifications", "id")]).await, notices_before, "listing never acknowledges notices");

        let (status, clamped) = request(&app, "GET", &format!("{BELL}?page=0&page_size=500"), Some(&cookie), Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(clamped["page"], 1);
        assert_eq!(clamped["page_size"], 50);
        for input in [
            json!({}), json!({"ids":[]}), json!({"ids":[0]}), json!({"ids":[-1]}),
            json!({"ids":[own[0],own[0]]}), json!({"ids":(1..=101).collect::<Vec<i64>>()}),
            json!({"through_id":0}), json!({"through_id":-1}), json!({"through_id":own[0]}),
            json!({"through_id":""}), json!({"through_id":"forged-snapshot"}),
            json!({"ids":[own[0]],"through_id":page["snapshot_id"]}), json!({"ids":[own[0]],"user_id":other}),
        ] {
            assert!(request(&app, "POST", READ, Some(&cookie), input).await.0.is_client_error());
        }
        assert!(request(&app, "GET", &format!("{BELL}?user_id={other}"), Some(&cookie), Value::Null).await.0.is_client_error());
        let another = another_session(&state, first).await;
        for wrong_cookie in [&foreign, &another] {
            assert!(request(&app, "POST", READ, Some(wrong_cookie), json!({"through_id":page["snapshot_id"]})).await.0.is_client_error(), "a snapshot is bound to its owner and issuing session");
        }
        assert!(request(&app, "POST", READ, Some(&cookie), json!({"through_id":format!("{}tampered",page["snapshot_id"].as_str().unwrap())})).await.0.is_client_error());
        for token in [
            scoped_fixture_snapshot(&state, first, sid, &own_rows, "notification_read", chrono::Utc::now().timestamp() - 10),
            scoped_fixture_snapshot(&state, first, sid, &own_rows, "website_auth", chrono::Utc::now().timestamp() + 900),
        ] {
            assert!(request(&app, "POST", READ, Some(&cookie), json!({"through_id":token})).await.0.is_client_error(), "expired or wrong-purpose signed claims cannot acknowledge notices");
        }
        let encoded = page["snapshot_id"].as_str().unwrap();
        for token in [encoded, encoded.strip_prefix("notification-snapshot.").unwrap()] {
            let forged_cookie = format!("illya_session={token}");
            assert_eq!(request(&app, "GET", "/api/v1/me", Some(&forged_cookie), Value::Null).await.0, StatusCode::UNAUTHORIZED, "notification signing scope cannot authorize website resources");
        }
        assert_eq!(rows(&state.db, &[("user_notifications", "id")]).await, notices_before);
        assert_eq!(read(&app, &cookie, json!({"ids":other_ids})).await, json!({"updated":0,"unread_count":2}));
        assert_eq!(read(&app, &cookie, json!({"ids":[own[0]]})).await, json!({"updated":1,"unread_count":1}));
        let acknowledged = rows(&state.db, &[("user_notifications", "id")]).await;
        assert_eq!(read(&app, &cookie, json!({"ids":[own[0]]})).await, json!({"updated":0,"unread_count":1}));
        assert_eq!(rows(&state.db, &[("user_notifications", "id")]).await, acknowledged, "retry preserves the original read timestamp");
        assert_eq!(list(&app, &foreign).await["unread_count"], 1);
        assert_eq!(chat_snapshot(&state.db).await, chat_before, "bell reads must not advance chat cursors or PM receipts");
    }).await;
    fixture.finish(outcome).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn notification_mark_all_is_bounded_and_keeps_post_snapshot_arrivals_unread() {
    let fixture = MutationFixture::new("bell_snapshot", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let outcome = tokio::spawn(async move {
        let (user, _, _, cookie) = actor(&state, "BellSnapshotOwner", &[]).await;
        let (_, _, _, other) = actor(&state, "BellSnapshotOther", &[]).await;
        // A legitimate 1000-member token must fit the route's request limit
        // even when sequence IDs have grown beyond the first few digits.
        sqlx::query("SELECT setval(pg_get_serial_sequence('user_notifications','id'),10000000000,false)")
            .execute(&state.db).await.unwrap();
        let original = seed(&state.db, user, "Snapshot batch", 1005).await;
        let snapshot = list(&app, &cookie).await;
        assert_eq!(snapshot["total"], 1005);
        assert_eq!(snapshot["snapshot_size"], 1000);
        assert_eq!(snapshot["items"].as_array().unwrap().len(), 20);
        let later = seed(&state.db, user, "Later publication", 1).await;
        assert!(later[0] > *original.iter().max().unwrap());
        let input = json!({"through_id":snapshot["snapshot_id"]});
        assert_eq!(read(&app, &cookie, input.clone()).await, json!({"updated":1000,"unread_count":6}));
        let after = rows(&state.db, &[("user_notifications", "id")]).await;
        assert_eq!(read(&app, &cookie, input).await, json!({"updated":0,"unread_count":6}));
        assert_eq!(rows(&state.db, &[("user_notifications", "id")]).await, after);
        assert_eq!(read(&app, &cookie, json!({"ids":&original[1000..]})).await, json!({"updated":5,"unread_count":1}));
        assert_eq!(list(&app, &other).await["total"], 0);
        let page = list(&app, &cookie).await;
        assert_eq!(page["items"][0]["id"], later[0]);
        assert!(page["items"][0]["read_at"].is_null());

        // PostgreSQL IDs are assigned before commit. A lower ID first becoming
        // visible after GET is also a new arrival and must not be acknowledged.
        let source = published_source(&state.db, user, "Deferred publication").await;
        let mut delayed = state.db.begin().await.unwrap();
        let lower: i64 = sqlx::query_scalar(
            "INSERT INTO user_notifications(user_id,event_key,kind,announcement_id,publication_at,title) \
             SELECT $1,'deferred-commit','announcement_published',id,published_at,title FROM announcements WHERE id=$2 RETURNING id",
        ).bind(user).bind(source).fetch_one(&mut *delayed).await.unwrap();
        let higher = seed(&state.db, user, "Committed publication", 1).await;
        assert!(lower < higher[0]);
        let observed = list(&app, &cookie).await;
        assert!(!observed["items"].as_array().unwrap().iter().any(|n| n["id"] == lower));
        delayed.commit().await.unwrap();
        let result = read(&app, &cookie, json!({"through_id":observed["snapshot_id"]})).await;
        assert_eq!(result, json!({"updated":2,"unread_count":1}), "an unobserved lower-ID commit remains unread");
        let deferred_read: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT read_at FROM user_notifications WHERE id=$1")
            .bind(lower).fetch_one(&state.db).await.unwrap();
        assert!(deferred_read.is_none());
        let explicit = seed(&state.db, user, "Explicit boundary", 100).await;
        assert_eq!(read(&app, &cookie, json!({"ids":explicit})).await, json!({"updated":100,"unread_count":1}), "the inclusive 100-ID boundary is accepted");
    }).await;
    fixture.finish(outcome).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn notification_publication_epochs_hide_withdrawals_and_exclude_future_accounts() {
    let fixture = MutationFixture::new("bell_publish", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let application = fixture.schema.clone();
    let outcome = tokio::spawn(async move {
        let (_, _, _, manager) = actor(&state, "BellPublisher", &["announcements.manage"]).await;
        let (recipient, _, _, cookie) = actor(&state, "BellPublicationRecipient", &[]).await;
        let (restricted, _, _, _) = actor(&state, "BellRestrictedRecipient", &[]).await;
        sqlx::query("UPDATE users SET is_restricted=true WHERE id=$1")
            .bind(restricted)
            .execute(&state.db)
            .await
            .unwrap();
        let draft = create_announcement(&app, &manager, "Draft title").await;
        assert_eq!(list(&app, &cookie).await["total"], 0);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM user_notifications")
                .fetch_one(&state.db)
                .await
                .unwrap(),
            0
        );
        let (status, published) = update_announcement(&app, &manager, &draft, "published").await;
        assert_eq!(status, StatusCode::OK, "{published}");
        let notice = list(&app, &cookie).await;
        assert_eq!(notice["total"], 1);
        assert_eq!(notice["items"][0]["kind"], "announcement_published");
        assert_eq!(notice["items"][0]["href"], format!("/news/{}", draft["id"]));
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM user_notifications WHERE user_id=$1"
            )
            .bind(restricted)
            .fetch_one(&state.db)
            .await
            .unwrap(),
            0
        );
        let (_, _, _, future) = actor(&state, "BellFutureRecipient", &[]).await;
        assert_eq!(
            list(&app, &future).await["total"],
            0,
            "registration never backfills old publications"
        );
        let first_id = notice["items"][0]["id"].as_i64().unwrap();
        read(&app, &cookie, json!({"ids":[first_id]})).await;
        let first_row: Value =
            sqlx::query_scalar("SELECT to_jsonb(n) FROM user_notifications n WHERE id=$1")
                .bind(first_id)
                .fetch_one(&state.db)
                .await
                .unwrap();
        let mut tx = state.db.begin().await.unwrap();
        crate::notifications::announcement_published(
            &mut tx,
            published["id"].as_i64().unwrap(),
            published["revision"].as_i64().unwrap(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            list(&app, &cookie).await["total"],
            1,
            "duplicate emission is idempotent"
        );
        let (status, edited) = update_announcement(&app, &manager, &published, "published").await;
        assert_eq!(status, StatusCode::OK, "{edited}");
        assert_eq!(edited["published_at"], published["published_at"]);
        assert_eq!(
            list(&app, &cookie).await["total"],
            1,
            "editing published content does not publish a new event"
        );
        assert_eq!(list(&app, &future).await["total"], 0);
        let (status, archived) = update_announcement(&app, &manager, &edited, "archived").await;
        assert_eq!(status, StatusCode::OK, "{archived}");
        assert_eq!(
            list(&app, &cookie).await["total"],
            0,
            "withdrawn source notices are not disclosed"
        );
        assert_eq!(
            read(&app, &cookie, json!({"ids":[first_id]})).await["updated"],
            0
        );
        let (status, republished) =
            update_announcement(&app, &manager, &archived, "published").await;
        assert_eq!(status, StatusCode::OK, "{republished}");
        assert_ne!(republished["published_at"], published["published_at"]);
        let active = list(&app, &cookie).await;
        assert_eq!(active["total"], 1);
        assert_eq!(active["unread_count"], 1);
        assert_ne!(
            active["items"][0]["id"], first_id,
            "old publication never resurfaces"
        );
        assert_eq!(
            list(&app, &future).await["total"],
            1,
            "a later publication is eligible after joining"
        );
        let preserved: Value =
            sqlx::query_scalar("SELECT to_jsonb(n) FROM user_notifications n WHERE id=$1")
                .bind(first_id)
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(
            preserved, first_row,
            "publication changes preserve historical read state"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM user_notifications WHERE user_id=$1"
            )
            .bind(recipient)
            .fetch_one(&state.db)
            .await
            .unwrap(),
            2
        );

        // Each publisher has its own actor row locked while fan-out targets
        // both publishers. Their recipient FKs must not deadlock these writes.
        let (_, _, _, left) = actor(
            &state,
            "BellConcurrentPublisherLeft",
            &["announcements.manage"],
        )
        .await;
        let (_, _, _, right) = actor(
            &state,
            "BellConcurrentPublisherRight",
            &["announcements.manage"],
        )
        .await;
        let first = create_announcement(&app, &left, "Concurrent first").await;
        let second = create_announcement(&app, &right, "Concurrent second").await;
        let mut blocker = state.db.begin().await.unwrap();
        sqlx::query("SELECT id FROM announcements WHERE id=ANY($1) ORDER BY id FOR UPDATE")
            .bind(vec![
                first["id"].as_i64().unwrap(),
                second["id"].as_i64().unwrap(),
            ])
            .fetch_all(&mut *blocker)
            .await
            .unwrap();
        let left_app = app.clone();
        let left_cookie = left.clone();
        let left_job = tokio::spawn(async move {
            update_announcement(&left_app, &left_cookie, &first, "published").await
        });
        let right_app = app.clone();
        let right_cookie = right.clone();
        let right_job = tokio::spawn(async move {
            update_announcement(&right_app, &right_cookie, &second, "published").await
        });
        wait_for_both_publishers(&state.db, &application).await;
        blocker.commit().await.unwrap();
        let (first_result, second_result) = tokio::time::timeout(Duration::from_secs(5), async {
            let (first, second) = tokio::join!(left_job, right_job);
            (first.unwrap(), second.unwrap())
        })
        .await
        .expect("concurrent fan-out must not deadlock publishers");
        assert_eq!(first_result.0, StatusCode::OK, "{}", first_result.1);
        assert_eq!(second_result.0, StatusCode::OK, "{}", second_result.1);
        assert_eq!(list(&app, &left).await["total"], 2);
        assert_eq!(list(&app, &right).await["total"], 2);
    })
    .await;
    fixture.finish(outcome).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn notification_rank_decisions_target_only_the_requester_and_retry_once() {
    let fixture = MutationFixture::new("bell_rank", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let outcome = tokio::spawn(async move {
        let (reviewer_id, _, _, reviewer) = actor(
            &state,
            "BellRankReviewer",
            &["beatmaps.rank.review", "beatmaps.moderate"],
        )
        .await;
        let (_, _, _, bystander) = actor(&state, "BellRankBystander", &["*"]).await;
        for decision in ["approved", "rejected"] {
            let (user, _, _, cookie) = actor(
                &state,
                &format!("BellRank{decision}"),
                &["beatmaps.rank.request"],
            )
            .await;
            let submitted = local_request(&state, &app, user, &cookie).await;
            assert_eq!(
                list(&app, &cookie).await["total"],
                0,
                "pending requests do not emit decisions"
            );
            let (status, decided) = decide(&app, &reviewer, &submitted, decision).await;
            assert_eq!(status, StatusCode::OK, "{decided}");
            assert_eq!(decided["status"], decision);
            let page = list(&app, &cookie).await;
            assert_eq!(page["total"], 1);
            assert_eq!(page["unread_count"], 1);
            assert_eq!(page["items"][0]["kind"], format!("rank_request_{decision}"));
            assert_eq!(page["items"][0]["message"], "Review complete");
            assert_eq!(
                page["items"][0]["href"],
                format!("/beatmapsets/local-{}", submitted["native_set_id"])
            );
            let recipients: Vec<i64> = sqlx::query_scalar("SELECT user_id FROM user_notifications WHERE rank_request_id=$1 ORDER BY user_id")
                .bind(submitted["id"].as_i64().unwrap()).fetch_all(&state.db).await.unwrap();
            assert_eq!(recipients, vec![user], "emission stores exactly the immutable requester as receiver");
            assert_eq!(list(&app, &reviewer).await["total"], 0);
            assert_eq!(list(&app, &bystander).await["total"], 0);
            let current: String =
                sqlx::query_scalar("SELECT status FROM beatmaps WHERE native_set_id=$1")
                    .bind(submitted["native_set_id"].as_i64().unwrap())
                    .fetch_one(&state.db)
                    .await
                    .unwrap();
            assert_eq!(
                current,
                if decision == "approved" {
                    "ranked"
                } else {
                    "graveyard"
                }
            );
            let id = page["items"][0]["id"].as_i64().unwrap();
            read(&app, &cookie, json!({"ids":[id]})).await;
            let before = source_snapshot(&state.db).await;
            assert_eq!(
                decide(&app, &reviewer, &submitted, decision).await.0,
                StatusCode::CONFLICT
            );
            assert_eq!(
                source_snapshot(&state.db).await,
                before,
                "a duplicate HTTP review publishes nothing"
            );
            let mut tx = state.db.begin().await.unwrap();
            crate::notifications::rank_request_decided(&mut tx, submitted["id"].as_i64().unwrap())
                .await
                .unwrap();
            tx.commit().await.unwrap();
            assert_eq!(
                source_snapshot(&state.db).await,
                before,
                "repeated emission preserves read state and source identity"
            );
        }
        let (official_user, _, _, official_cookie) = actor(&state, "BellOfficialRequester", &[]).await;
        let mut tx = state.db.begin().await.unwrap();
        let official_request: i64 = sqlx::query_scalar(
            "INSERT INTO beatmap_rank_requests(native_set_id,source,requester_id,reason,metadata,status,review_reason,reviewed_by,reviewed_at) \
             VALUES(725000,'official',$1,'Fixture nomination',$2,'rejected','Review complete',$3,clock_timestamp()) RETURNING id",
        ).bind(official_user).bind(json!({"title":"Official fixture","manifest":[]})).bind(reviewer_id)
            .fetch_one(&mut *tx).await.unwrap();
        crate::notifications::rank_request_decided(&mut tx, official_request).await.unwrap();
        tx.commit().await.unwrap();
        let official = list(&app, &official_cookie).await;
        assert_eq!(official["total"], 1);
        assert_eq!(official["items"][0]["href"], "/beatmapsets/725000", "official links retain the canonical numeric set identity");
        assert_eq!(list(&app, &bystander).await["total"], 0);
    })
    .await;
    fixture.finish(outcome).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn notification_routes_recheck_queued_sessions_and_expire_after_resource_waits() {
    let fixture = MutationFixture::new("bell_auth", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let application = fixture.schema.clone();
    let outcome = tokio::spawn(async move {
        for marking in [false, true] {
            for failure in ["revoked", "restricted", "expired"] {
                let (user, _, sid, cookie) = actor(&state, &format!("Bell{marking}{failure}"), &[]).await;
                let ids = seed(&state.db, user, &format!("Guard {marking} {failure}"), 1).await;
                let before = rows(&state.db, &[("user_notifications", "id")]).await;
                let expiry = if failure == "expired" {
                    Some(sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>("UPDATE sessions SET expires_at=clock_timestamp()+interval '1 second' WHERE id=$1 RETURNING expires_at")
                        .bind(sid).fetch_one(&state.db).await.unwrap())
                } else { None };
                let mut blocker = state.db.begin().await.unwrap();
                sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE").bind(user).fetch_one(&mut *blocker).await.unwrap();
                let queued_app = app.clone();
                let queued = tokio::spawn(async move {
                    request(&queued_app, if marking { "POST" } else { "GET" }, if marking { READ } else { BELL }, Some(&cookie), json!({"ids":ids})).await
                });
                wait_for_lock(&state.db, &application, "SELECT id FROM users WHERE id=ANY").await;
                match failure {
                    "revoked" => { sqlx::query("DELETE FROM sessions WHERE id=$1").bind(sid).execute(&mut *blocker).await.unwrap(); }
                    "restricted" => { sqlx::query("UPDATE users SET is_restricted=true WHERE id=$1").bind(user).execute(&mut *blocker).await.unwrap(); }
                    _ => { wait_until_expired(&state.db, expiry.unwrap()).await; }
                }
                blocker.commit().await.unwrap();
                let (status, value) = tokio::time::timeout(Duration::from_secs(5), queued).await.unwrap().unwrap();
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{marking}/{failure}: {value}");
                assert!(!value.to_string().contains("Guard"), "queued revoked GET never delivers a notice");
                assert_eq!(rows(&state.db, &[("user_notifications", "id")]).await, before);
            }
        }
        for marking in [false, true] {
            let (user, _, sid, cookie) = actor(&state, &format!("BellResource{marking}"), &[]).await;
            let ids = seed(&state.db, user, &format!("Resource deadline {marking}"), 1).await;
            let before = rows(&state.db, &[("user_notifications", "id")]).await;
            let expiry = sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>("UPDATE sessions SET expires_at=clock_timestamp()+interval '1 second' WHERE id=$1 RETURNING expires_at")
                .bind(sid).fetch_one(&state.db).await.unwrap();
            let mut blocker = state.db.begin().await.unwrap();
            sqlx::query("LOCK TABLE user_notifications IN ACCESS EXCLUSIVE MODE").execute(&mut *blocker).await.unwrap();
            let queued_app = app.clone();
            let queued = tokio::spawn(async move {
                request(&queued_app, if marking { "POST" } else { "GET" }, if marking { READ } else { BELL }, Some(&cookie), json!({"ids":ids})).await
            });
            wait_for_lock(&state.db, &application, "user_notifications").await;
            wait_until_expired(&state.db, expiry).await;
            blocker.commit().await.unwrap();
            let (status, value) = tokio::time::timeout(Duration::from_secs(5), queued).await.unwrap().unwrap();
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{marking}: {value}");
            assert_eq!(rows(&state.db, &[("user_notifications", "id")]).await, before, "post-lock expiry rolls back acknowledgement or suppresses delivery");
        }
        let (user, _, sid, cookie) = actor(&state, "BellSnapshotResourceExpiry", &[]).await;
        let ids = seed(&state.db, user, "Snapshot resource deadline", 1).await;
        let members = identities(&state.db, &ids).await;
        let before = rows(&state.db, &[("user_notifications", "id")]).await;
        // The session remains valid. Only the short-lived acknowledgement
        // expires while the selected notification row is locked elsewhere.
        let deadline: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
            "SELECT date_trunc('second',clock_timestamp())+interval '3 seconds'",
        ).fetch_one(&state.db).await.unwrap();
        let token = scoped_fixture_snapshot(&state, user, sid, &members, "notification_read", deadline.timestamp());
        let mut blocker = state.db.begin().await.unwrap();
        sqlx::query("SELECT id FROM user_notifications WHERE id=$1 FOR UPDATE")
            .bind(ids[0]).fetch_one(&mut *blocker).await.unwrap();
        let queued_app = app.clone();
        let queued = tokio::spawn(async move {
            request(&queued_app, "POST", READ, Some(&cookie), json!({"through_id":token})).await
        });
        wait_for_lock(&state.db, &application, "user_notifications").await;
        wait_until_expired(&state.db, deadline).await;
        // JWT validation uses the API process clock. Wait past its whole-second
        // boundary too, so minor fixture DB/process skew cannot weaken expiry.
        tokio::time::timeout(Duration::from_secs(5), async {
            while chrono::Utc::now().timestamp() <= deadline.timestamp() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("the notification snapshot must expire on the API clock");
        blocker.commit().await.unwrap();
        let (status, value) = tokio::time::timeout(Duration::from_secs(5), queued).await.unwrap().unwrap();
        assert_eq!(status, StatusCode::BAD_REQUEST, "a snapshot must remain valid after its resource wait: {value}");
        assert_eq!(rows(&state.db, &[("user_notifications", "id")]).await, before, "expired snapshot acknowledgement rolls back its row updates");
    }).await;
    fixture.finish(outcome).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn notification_sources_and_notices_rollback_together_on_failed_commit() {
    let fixture = MutationFixture::new("bell_rollback", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let outcome = tokio::spawn(async move {
        let (user, _, _, cookie) =
            actor(&state, "BellRollbackOwner", &["beatmaps.rank.request"]).await;
        let (_, _, _, manager) = actor(
            &state,
            "BellRollbackManager",
            &[
                "announcements.manage",
                "beatmaps.rank.review",
                "beatmaps.moderate",
            ],
        )
        .await;
        let submitted = local_request(&state, &app, user, &cookie).await;
        let draft = create_announcement(&app, &manager, "Rollback draft").await;
        assert_eq!(list(&app, &cookie).await["total"], 0);
        let before = source_snapshot(&state.db).await;
        reject_audits_at_commit(&state.db).await;
        for decision in ["approved", "rejected"] {
            let (status, _) = decide(&app, &manager, &submitted, decision).await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(
                source_snapshot(&state.db).await,
                before,
                "failed {decision} commit cannot publish a decision or its notice"
            );
            assert_eq!(list(&app, &cookie).await["total"], 0);
        }
        let (status, _) = update_announcement(&app, &manager, &draft, "published").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            source_snapshot(&state.db).await,
            before,
            "failed publication commit preserves draft, policies and all inboxes"
        );
        assert_eq!(list(&app, &cookie).await["total"], 0);
        assert_eq!(list(&app, &manager).await["total"], 0);
        let record = sqlx::query("SELECT status,revision FROM announcements WHERE id=$1")
            .bind(draft["id"].as_i64().unwrap())
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(record.get::<String, _>("status"), "draft");
        assert_eq!(record.get::<i64, _>("revision"), 1);
    })
    .await;
    fixture.finish(outcome).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn notification_snapshot_restoration_preserves_identity_without_acknowledging_reused_ids() {
    let fixture = MutationFixture::new("bell_restore", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let outcome = tokio::spawn(async move {
        let (user, _, _, cookie) = actor(&state, "BellRestoreOwner", &[]).await;
        let original = seed(&state.db, user, "Restore publication", 2).await;
        let initial = identities(&state.db, &original).await;
        let observed = list(&app, &cookie).await;
        assert_eq!(observed["snapshot_size"], 2);
        let reused = original[0];
        let preserved = original[1];
        let backup: Value = sqlx::query_scalar("SELECT to_jsonb(n) FROM user_notifications n WHERE id=$1")
            .bind(preserved).fetch_one(&state.db).await.unwrap();
        let source = backup["announcement_id"].as_i64().unwrap();

        // Simulate pg_restore preserving a backup row's full identity, then
        // its restored sequence reusing an absent post-backup event ID. This
        // owns only its fixture schema and is not a deployment restore test.
        let mut tx = state.db.begin().await.unwrap();
        sqlx::query("DELETE FROM user_notifications WHERE id=ANY($1)")
            .bind(&original).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO user_notifications SELECT (jsonb_populate_record(NULL::user_notifications,$1::jsonb)).*")
            .bind(&backup).execute(&mut *tx).await.unwrap();
        sqlx::query("SELECT setval(pg_get_serial_sequence('user_notifications','id'),$1,false)")
            .bind(reused).execute(&mut *tx).await.unwrap();
        let (new_id, new_identity): (i64, Uuid) = sqlx::query_as(
            "INSERT INTO user_notifications(user_id,event_key,kind,announcement_id,publication_at,title) \
             SELECT $1,'after-restore-new-event','announcement_published',a.id,a.published_at,'New event after restore' \
             FROM announcements a WHERE a.id=$2 RETURNING id,identity",
        ).bind(user).bind(source).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(new_id, reused, "the owned restored sequence reuses the missing ID");
        assert_ne!(new_identity, initial[0].1);
        assert_eq!(identities(&state.db, &[preserved]).await, vec![initial[1]], "the backup row retains its original event identity");

        let change = sqlx::query("UPDATE user_notifications SET identity=gen_random_uuid() WHERE id=$1")
            .bind(reused).execute(&state.db).await.unwrap_err();
        assert_eq!(change.as_database_error().unwrap().code().as_deref(), Some("23514"), "event identity updates violate the immutable identity constraint");
        assert_eq!(identities(&state.db, &[reused]).await, vec![(reused, new_identity)]);
        let result = read(&app, &cookie, json!({"through_id":observed["snapshot_id"]})).await;
        assert_eq!(result, json!({"updated":1,"unread_count":1}), "the snapshot acknowledges only the unchanged restored row");
        let new_read: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT read_at FROM user_notifications WHERE id=$1")
            .bind(reused).fetch_one(&state.db).await.unwrap();
        let restored_read: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT read_at FROM user_notifications WHERE id=$1")
            .bind(preserved).fetch_one(&state.db).await.unwrap();
        assert!(new_read.is_none(), "an unobserved event with a reused numeric ID stays unread");
        assert!(restored_read.is_some(), "a restored observed event remains acknowledgeable");
        assert_eq!(read(&app, &cookie, json!({"through_id":observed["snapshot_id"]})).await, json!({"updated":0,"unread_count":1}));
        assert_eq!(read(&app, &cookie, json!({"ids":[reused]})).await, json!({"updated":1,"unread_count":0}), "a fresh explicit acknowledgement can read the new event");
    }).await;
    fixture.finish(outcome).await;
}
