//! Review revisions cover competing HTTP decisions and other database writers.
use super::*;
use crate::maps::map_mutation_tests::{
    MutationFixture, actor, reject_audits_at_commit, wait_for_lock, wait_until_expired,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, header},
};
use chrono::{DateTime, Utc};
use std::time::Duration;
use tower::ServiceExt;

async fn request(
    app: &Router,
    method: Method,
    path: &str,
    cookie: &str,
    input: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::ORIGIN, "http://localhost:3000")
        .header(header::COOKIE, cookie);
    let body = if let Some(input) = input {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(input.to_string())
    } else {
        Body::empty()
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
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

async fn score(state: &AppState, owner: i64) -> i64 {
    let map: i64 = sqlx::query_scalar("INSERT INTO beatmaps(title,artist,creator,version,mode,stars,md5,osu_path) VALUES('Review fixture','Illyaverse','Tests','Easy',0,1,$1,'unread-fixture.osu') RETURNING id")
        .bind(format!("{:x}",md5::compute(b"score-moderation-fixture")))
        .fetch_one(&state.db).await.unwrap();
    sqlx::query_scalar("INSERT INTO scores(user_id,beatmap_id,mode,score,accuracy,max_combo,pp) VALUES($1,$2,0,1000,100,1,1) RETURNING id")
        .bind(owner).bind(map).fetch_one(&state.db).await.unwrap()
}

async fn row(db: &sqlx::PgPool, id: i64) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('verification',verification,'moderation_revision',moderation_revision) FROM scores WHERE id=$1")
        .bind(id).fetch_one(db).await.unwrap()
}
async fn audits(db: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='scores.verify'")
        .fetch_one(db)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn score_moderation_cas_rejects_concurrent_reviews_and_database_aba() {
    let fixture = MutationFixture::new("score_review_cas", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let application = fixture.schema.clone();
    let result = tokio::spawn(async move {
        let (first, _, _, first_cookie) =
            actor(&state, "FirstReviewer", &["scores.moderate"]).await;
        let (_, _, _, second_cookie) = actor(&state, "SecondReviewer", &["scores.moderate"]).await;
        let (_, _, _, unprivileged) = actor(&state, "NormalReviewer", &["admin.view"]).await;
        let id = score(&state, first).await;
        let path = format!("/api/v1/admin/scores/{id}");
        let (code, unchanged) = request(
            &app,
            Method::PATCH,
            &path,
            &second_cookie,
            Some(json!({"verification":"pending","expected_revision":0})),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(unchanged["error"]["code"], "score_verification_unchanged");
        assert_eq!(
            audits(&state.db).await,
            0,
            "a no-op cannot create a native moderation decision without a revision"
        );
        assert_eq!(
            request(
                &app,
                Method::PATCH,
                &path,
                &unprivileged,
                Some(json!({"verification":"verified","expected_revision":0}))
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let (code, detail) = request(
            &app,
            Method::GET,
            &format!("/api/v1/scores/{id}"),
            &second_cookie,
            None,
        )
        .await;
        assert_eq!(
            code,
            StatusCode::OK,
            "scores.moderate needs no admin.view: {detail}"
        );
        assert_eq!(detail["moderation_revision"], 0);
        let (code, list) = request(
            &app,
            Method::GET,
            "/api/v1/scores?verification=pending",
            &second_cookie,
            None,
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{list}");
        assert_eq!(list["items"][0]["moderation_revision"], 0);
        let mut blocking = state.db.begin().await.unwrap();
        sqlx::query("SELECT id FROM scores WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *blocking)
            .await
            .unwrap();
        let first_app = app.clone();
        let first_path = path.clone();
        let first_request = tokio::spawn(async move {
            request(
                &first_app,
                Method::PATCH,
                &first_path,
                &first_cookie,
                Some(json!({"verification":"verified","expected_revision":0})),
            )
            .await
        });
        let second_app = app.clone();
        let second_path = path.clone();
        let second_copy = second_cookie.clone();
        let second_request = tokio::spawn(async move {
            request(
                &second_app,
                Method::PATCH,
                &second_path,
                &second_copy,
                Some(json!({"verification":"rejected","expected_revision":0})),
            )
            .await
        });
        wait_for_lock(
            &state.db,
            &application,
            "FROM scores WHERE id=$1 FOR UPDATE",
        )
        .await;
        blocking.commit().await.unwrap();
        let results = [first_request.await.unwrap(), second_request.await.unwrap()];
        assert_eq!(
            results
                .iter()
                .filter(|(code, _)| *code == StatusCode::OK)
                .count(),
            1,
            "{results:?}"
        );
        assert_eq!(
            results
                .iter()
                .filter(|(code, _)| *code == StatusCode::CONFLICT)
                .count(),
            1,
            "{results:?}"
        );
        assert_eq!(audits(&state.db).await, 1);
        let stored = row(&state.db, id).await;
        assert_eq!(stored["moderation_revision"], 1);
        let details: Value =
            sqlx::query_scalar("SELECT details FROM audit_events WHERE action='scores.verify'")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(
            details["previous"],
            json!({"verification":"pending","moderation_revision":0})
        );
        assert_eq!(details["new"], stored);
        let winner = stored["verification"].as_str().unwrap();
        let intermediate = if winner == "verified" {
            "rejected"
        } else {
            "verified"
        };
        // Native/recovery writers also change the revision, and cannot force it
        // backward by explicitly supplying an old revision during either step.
        sqlx::query("UPDATE scores SET verification=$2,moderation_revision=0 WHERE id=$1")
            .bind(id)
            .bind(intermediate)
            .execute(&state.db)
            .await
            .unwrap();
        sqlx::query("UPDATE scores SET verification=$2,moderation_revision=0 WHERE id=$1")
            .bind(id)
            .bind(winner)
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(
            row(&state.db, id).await,
            json!({"verification":winner,"moderation_revision":3})
        );
        let (code, value) = request(
            &app,
            Method::PATCH,
            &path,
            &second_cookie,
            Some(json!({"verification":"pending","expected_revision":1})),
        )
        .await;
        assert_eq!(
            code,
            StatusCode::CONFLICT,
            "ABA must not make an old decision fresh: {value}"
        );
        assert_eq!(value["error"]["code"], "score_moderation_conflict");
        assert_eq!(audits(&state.db).await, 1);
        sqlx::query("UPDATE scores SET pp=5,moderation_revision=0 WHERE id=$1")
            .bind(id)
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(
            row(&state.db, id).await["moderation_revision"],
            3,
            "unrelated cache updates preserve revision"
        );
        assert_eq!(
            request(
                &app,
                Method::PATCH,
                &path,
                &second_cookie,
                Some(json!({"verification":"pending","expected_revision":3}))
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            row(&state.db, id).await,
            json!({"verification":"pending","moderation_revision":4})
        );
        assert_eq!(audits(&state.db).await, 2);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn queued_score_review_rechecks_live_authorization_and_session_deadlines() {
    let fixture = MutationFixture::new("score_auth", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let application = fixture.schema.clone();
    let result = tokio::spawn(async move {
        let (owner,_,_,_) = actor(&state,"ReviewOwner",&["chat.send"]).await;
        let id = score(&state,owner).await;
        for (index,change) in ["permission","session","restriction"].into_iter().enumerate() {
            let (reviewer,role,sid,cookie) = actor(&state,&format!("QueuedReviewer{index}"),&["scores.moderate"]).await;
            let mut blocking = state.db.begin().await.unwrap();
            sqlx::query("LOCK TABLE roles,user_roles IN SHARE ROW EXCLUSIVE MODE").execute(&mut *blocking).await.unwrap();
            let queued_app = app.clone();
            let queued = tokio::spawn(async move { request(&queued_app,Method::PATCH,&format!("/api/v1/admin/scores/{id}"),&cookie,Some(json!({"verification":"verified","expected_revision":0}))).await });
            wait_for_lock(&state.db,&application,"LOCK TABLE roles,user_roles IN SHARE ROW EXCLUSIVE MODE").await;
            match change {
                "permission" => {sqlx::query("UPDATE roles SET permissions='{}' WHERE id=$1").bind(role).execute(&mut *blocking).await.unwrap();}
                "session" => {sqlx::query("DELETE FROM sessions WHERE id=$1").bind(sid).execute(&mut *blocking).await.unwrap();}
                "restriction" => {sqlx::query("UPDATE users SET is_restricted=true WHERE id=$1").bind(reviewer).execute(&mut *blocking).await.unwrap();}
                _ => unreachable!(),
            }
            blocking.commit().await.unwrap();
            let (code,value) = tokio::time::timeout(Duration::from_secs(5),queued).await.unwrap().unwrap();
            assert_eq!(code,if change=="permission" {StatusCode::FORBIDDEN} else {StatusCode::UNAUTHORIZED},"{change}: {value}");
            assert_eq!(row(&state.db,id).await,json!({"verification":"pending","moderation_revision":0}));
            assert_eq!(audits(&state.db).await,0);
        }
        let (_,_,sid,cookie) = actor(&state,"DeadlineReviewer",&["scores.moderate"]).await;
        let expiry: DateTime<Utc> = sqlx::query_scalar("UPDATE sessions SET expires_at=clock_timestamp()+interval '1500 milliseconds' WHERE id=$1 RETURNING expires_at")
            .bind(sid).fetch_one(&state.db).await.unwrap();
        let mut blocking = state.db.begin().await.unwrap();
        sqlx::query("SELECT id FROM scores WHERE id=$1 FOR UPDATE").bind(id).fetch_one(&mut *blocking).await.unwrap();
        let queued_app = app.clone();
        let queued = tokio::spawn(async move { request(&queued_app,Method::PATCH,&format!("/api/v1/admin/scores/{id}"),&cookie,Some(json!({"verification":"verified","expected_revision":0}))).await });
        wait_for_lock(&state.db,&application,"FROM scores WHERE id=$1 FOR UPDATE").await;
        wait_until_expired(&state.db,expiry).await;
        blocking.commit().await.unwrap();
        assert_eq!(queued.await.unwrap().0,StatusCode::UNAUTHORIZED);
        assert_eq!(row(&state.db,id).await,json!({"verification":"pending","moderation_revision":0}));
        assert_eq!(audits(&state.db).await,0);
    }).await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn score_moderation_audit_failure_rolls_back_decision_and_revision() {
    let fixture = MutationFixture::new("score_review_rollback", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (owner, _, _, cookie) = actor(&state, "RollbackReviewer", &["scores.moderate"]).await;
        let id = score(&state, owner).await;
        reject_audits_at_commit(&state.db).await;
        assert_eq!(
            request(
                &app,
                Method::PATCH,
                &format!("/api/v1/admin/scores/{id}"),
                &cookie,
                Some(json!({"verification":"verified","expected_revision":0}))
            )
            .await
            .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            row(&state.db, id).await,
            json!({"verification":"pending","moderation_revision":0})
        );
        assert_eq!(audits(&state.db).await, 0);
    })
    .await;
    fixture.finish(result).await;
}
