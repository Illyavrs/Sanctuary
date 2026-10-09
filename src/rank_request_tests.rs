//! Ranking requests use owned PG17 schemas and synthetic loopback upstreams.
use crate::{
    AppState, Router,
    maps::map_mutation_tests::{
        MutationFixture, actor, file_count, reject_audits_at_commit, wait_for_lock,
        wait_until_expired,
    },
    native_map_tests::OfficialServer,
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

const CREATE: &str = "/api/v1/beatmap-rank-requests";
const MINE: &str = "/api/v1/me/beatmap-rank-requests";
const QUEUE: &str = "/api/v1/admin/beatmap-rank-requests";
const REQUEST_PERMISSION: &str = "beatmaps.rank.request";
const REVIEW_PERMISSION: &str = "beatmaps.rank.review";

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
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn local_set(state: &AppState, owner: i64, statuses: &[&str]) -> i64 {
    assert!(!statuses.is_empty());
    let package = Uuid::new_v4();
    let mut manifest = statuses
        .iter()
        .enumerate()
        .map(|(index, _)| format!("{:x}", md5::compute(format!("{package}/{index}"))))
        .collect::<Vec<_>>();
    manifest.sort_unstable();
    let archive = format!("private-fixture-{package}/package.osz");
    let mut tx = state.db.begin().await.unwrap();
    let set = sqlx::query_scalar("INSERT INTO beatmap_packages(id,uploaded_by,kind,member_md5,archive_path,legacy) VALUES($1,$2,'osz',$3,$4,true) RETURNING native_set_id")
        .bind(package).bind(owner).bind(&manifest).bind(&archive)
        .fetch_one(&mut *tx).await.unwrap();
    for (index, status) in statuses.iter().enumerate() {
        sqlx::query("INSERT INTO beatmaps(local_package_id,uploaded_by,title,artist,creator,version,mode,status,stars,md5,osu_path,archive_path) VALUES($1,$2,'Rank fixture','Fixture Artist','Fixture Creator',$3,0,$4,2,$5,$6,$7)")
            .bind(package).bind(owner).bind(format!("Difficulty {index}"))
            .bind(status).bind(&manifest[index])
            .bind(format!("private-fixture-{package}/{index}.osu")).bind(&archive)
            .execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    set
}

async fn create_local(app: &Router, cookie: &str, set: i64) -> (StatusCode, Value) {
    request(
        app,
        "POST",
        CREATE,
        Some(cookie),
        json!({"native_set_id":set,"reason":"  Please review this set  "}),
    )
    .await
}

async fn withdraw(app: &Router, cookie: &str, id: i64, revision: i64) -> (StatusCode, Value) {
    request(
        app,
        "POST",
        &format!("{CREATE}/{id}/withdraw"),
        Some(cookie),
        json!({"expected_revision":revision}),
    )
    .await
}

async fn decide(
    app: &Router,
    cookie: &str,
    id: i64,
    decision: &str,
    revision: i64,
    difficulties: &Value,
) -> (StatusCode, Value) {
    let mut input =
        json!({"decision":decision,"reason":"Review complete","expected_revision":revision});
    if decision == "approved" {
        let selected = &difficulties[0];
        input["difficulty_ids"] = json!([selected["native_id"]]);
        input["difficulty_revisions"] = json!([{
            "native_id": selected["native_id"],
            "expected_revision": selected["revision"],
            "expected_status_revision": selected["status_revision"],
        }]);
    }
    request(app, "PATCH", &format!("{QUEUE}/{id}"), Some(cookie), input).await
}

async fn snapshot(state: &AppState) -> Value {
    let mut result = serde_json::Map::new();
    for (table, order) in [
        ("beatmap_rank_requests", "id"),
        ("beatmap_status_overrides", "native_id"),
        ("beatmaps", "id"),
        ("beatmap_packages", "id"),
        ("beatmapset_availability", "native_set_id"),
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

async fn stored(state: &AppState, id: i64) -> Value {
    sqlx::query_scalar("SELECT to_jsonb(r) FROM beatmap_rank_requests r WHERE id=$1")
        .bind(id)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

fn assert_public(value: &Value) {
    match value {
        Value::Object(fields) => {
            for (key, child) in fields {
                assert!(
                    ![
                        "email",
                        "password_hash",
                        "password_md5_hash",
                        "member_md5",
                        "manifest",
                        "md5",
                        "osu_path",
                        "archive_path",
                        "audit_events",
                        "sha256",
                        "client_secret"
                    ]
                    .contains(&key.as_str()),
                    "private field {key}: {value}"
                );
                assert_public(child);
            }
        }
        Value::Array(values) => values.iter().for_each(assert_public),
        _ => {}
    }
}

fn created(value: &Value, set: i64, source: &str) -> i64 {
    let id = value["id"].as_i64().expect("request DTO has an ID");
    assert_eq!(value["native_set_id"], set);
    assert_eq!(value["source"], source);
    assert_eq!(value["status"], "pending");
    assert_eq!(value["revision"], 0);
    assert_eq!(value["reason"], "Please review this set");
    assert_public(value);
    id
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn rank_request_http_guards_and_owner_lists_do_not_expose_other_players() {
    let fixture = MutationFixture::new("rank_req_guards", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (owner, _, _, cookie) = actor(&state, "RankRequestOwner", &[REQUEST_PERMISSION]).await;
        let (other, _, _, other_cookie) =
            actor(&state, "RankRequestOther", &[REQUEST_PERMISSION]).await;
        let (_, _, _, denied) = actor(
            &state,
            "RankRequestDenied",
            &["beatmaps.upload", "admin.view"],
        )
        .await;
        let (_, _, _, reviewer) = actor(&state, "RankRequestReviewer", &[REVIEW_PERMISSION]).await;
        let set = local_set(&state, owner, &["graveyard", "pending"]).await;
        let other_set = local_set(&state, other, &["unranked"]).await;
        let before = snapshot(&state).await;
        assert_eq!(
            request(
                &app,
                "POST",
                CREATE,
                None,
                json!({"native_set_id":set,"reason":"Review"})
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            create_local(&app, &denied, set).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(&app, "GET", QUEUE, Some(&denied), Value::Null)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        for input in [
            json!({"reason":"Review"}),
            json!({"native_set_id":set,"official_set_id":42,"reason":"Review"}),
            json!({"official_set_id":set,"reason":"Review"}),
            json!({"native_set_id":set,"reason":" "}),
            json!({"native_set_id":set,"reason":"unsafe\nreason"}),
            json!({"native_set_id":set,"reason":"x".repeat(1001)}),
            json!({"native_set_id":set,"reason":"Review","status":"approved"}),
        ] {
            let status = request(&app, "POST", CREATE, Some(&cookie), input.clone())
                .await
                .0;
            assert!(
                matches!(
                    status,
                    StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
                ),
                "{status}: {input}"
            );
        }
        assert_eq!(snapshot(&state).await, before);
        let (status, value) = create_local(&app, &cookie, set).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let id = created(&value, set, "local");
        let (status, other_value) = create_local(&app, &other_cookie, other_set).await;
        assert_eq!(status, StatusCode::OK, "{other_value}");
        let other_id = created(&other_value, other_set, "local");
        for cookie in [&other_cookie, &denied] {
            assert_eq!(
                request(
                    &app,
                    "GET",
                    &format!("{CREATE}/{id}"),
                    Some(cookie),
                    Value::Null
                )
                .await
                .0,
                StatusCode::NOT_FOUND
            );
        }
        for cookie in [&cookie, &reviewer] {
            let (status, value) = request(
                &app,
                "GET",
                &format!("{CREATE}/{id}"),
                Some(cookie),
                Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{value}");
            assert_eq!(value["id"], id);
            assert_public(&value);
        }
        assert_eq!(withdraw(&app, &cookie, id, 0).await.0, StatusCode::OK);
        let (status, mine) = request(&app, "GET", MINE, Some(&cookie), Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{mine}");
        assert_eq!(mine["total"], 1);
        assert_eq!(mine["items"][0]["id"], id);
        assert_eq!(
            mine["items"][0]["status"], "withdrawn",
            "owner list includes closed requests by default"
        );
        assert_public(&mine);
        let (status, pending) = request(
            &app,
            "GET",
            &format!("{MINE}?status=pending"),
            Some(&cookie),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(pending["total"], 0);
        let (status, queue) = request(&app, "GET", QUEUE, Some(&reviewer), Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{queue}");
        assert_eq!(queue["total"], 1);
        assert_eq!(queue["items"][0]["id"], other_id);
        assert_public(&queue);
        let (status, filtered) = request(
            &app,
            "GET",
            &format!("{MINE}?native_set_id={other_set}"),
            Some(&cookie),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            filtered["total"], 0,
            "set selector never widens owner scope"
        );
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn rank_requests_require_own_complete_unpublished_active_local_sets() {
    let fixture = MutationFixture::new("rank_req_admit", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result=tokio::spawn(async move {
        let (owner,_,_,cookie)=actor(&state,"RankAdmissionOwner",&[REQUEST_PERMISSION]).await;
        let (foreign,_,_,_)=actor(&state,"RankAdmissionForeign",&[REQUEST_PERMISSION]).await;
        let foreign_set=local_set(&state,foreign,&["graveyard"]).await;
        let published=local_set(&state,owner,&["ranked","ranked"]).await;
        let loved=local_set(&state,owner,&["loved","loved"]).await;
        let withdrawn=local_set(&state,owner,&["graveyard"]).await;
        sqlx::query("INSERT INTO beatmapset_availability(native_set_id,withdrawn,revision,reason,actor_id) VALUES($1,true,1,'Fixture withdrawal',$2)")
            .bind(withdrawn).bind(owner).execute(&state.db).await.unwrap();
        let before=snapshot(&state).await;
        assert_eq!(create_local(&app,&cookie,foreign_set).await.0,StatusCode::BAD_REQUEST);
        for set in [published,loved] {
            let (status,value)=create_local(&app,&cookie,set).await;
            assert_eq!(status,StatusCode::BAD_REQUEST,"published member forbids requesting the set: {value}");
        }
        assert_eq!(create_local(&app,&cookie,withdrawn).await.0,StatusCode::GONE);
        assert_eq!(snapshot(&state).await,before);
        let eligible=local_set(&state,owner,&["pending","unranked","graveyard"]).await;
        let (status,value)=create_local(&app,&cookie,eligible).await;
        assert_eq!(status,StatusCode::OK,"{value}");
        created(&value,eligible,"local");
        assert_eq!(value["metadata"]["difficulty_count"],3);
        let mixed=local_set(&state,owner,&["ranked","pending"]).await;
        let published_native:i64=sqlx::query_scalar("SELECT native_id FROM beatmaps WHERE native_set_id=$1 AND status='ranked'")
            .bind(mixed).fetch_one(&state.db).await.unwrap();
        let before=snapshot(&state).await;
        assert_eq!(request(&app,"POST",CREATE,Some(&cookie),json!({"native_set_id":mixed,"difficulty_ids":[published_native],"reason":"Review"})).await.0,StatusCode::BAD_REQUEST);
        assert_eq!(snapshot(&state).await,before);
        let (status,value)=create_local(&app,&cookie,mixed).await;
        assert_eq!(status,StatusCode::OK,"{value}");
        assert_eq!(value["difficulties"].as_array().unwrap().len(),1,"published siblings are not silently nominated");
        assert_eq!(value["difficulties"][0]["status"],"pending");
    }).await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn rank_request_withdraw_and_review_use_cas_including_database_aba() {
    let fixture = MutationFixture::new("rank_req_cas", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (owner, _, _, cookie) = actor(&state, "RankCasOwner", &[REQUEST_PERMISSION]).await;
        let (_, _, _, other) = actor(&state, "RankCasOther", &[REQUEST_PERMISSION]).await;
        let (_, _, _, reviewer) = actor(
            &state,
            "RankCasReviewer",
            &[REVIEW_PERMISSION, "beatmaps.moderate"],
        )
        .await;
        let set = local_set(&state, owner, &["graveyard"]).await;
        let (status, value) = create_local(&app, &cookie, set).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let id = created(&value, set, "local");
        let before = snapshot(&state).await;
        assert_eq!(withdraw(&app, &other, id, 0).await.0, StatusCode::NOT_FOUND);
        assert_eq!(
            withdraw(&app, &reviewer, id, 0).await.0,
            StatusCode::NOT_FOUND,
            "review authority cannot withdraw someone else's request"
        );
        assert_eq!(withdraw(&app, &cookie, id, 1).await.0, StatusCode::CONFLICT);
        assert_eq!(snapshot(&state).await, before);
        // A non-HTTP writer cannot reset the revision when restoring the same
        // visible pending state after an intermediate decision.
        sqlx::query("UPDATE beatmap_rank_requests SET status='rejected',revision=0 WHERE id=$1")
            .bind(id)
            .execute(&state.db)
            .await
            .unwrap();
        sqlx::query("UPDATE beatmap_rank_requests SET status='pending',revision=0 WHERE id=$1")
            .bind(id)
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(stored(&state, id).await["revision"], 2);
        let before = snapshot(&state).await;
        assert_eq!(withdraw(&app, &cookie, id, 0).await.0, StatusCode::CONFLICT);
        assert_eq!(
            decide(&app, &reviewer, id, "approved", 0, &value["difficulties"])
                .await
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(snapshot(&state).await, before);
        let (status, value) = withdraw(&app, &cookie, id, 2).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["status"], "withdrawn");
        assert_eq!(value["revision"], 3);
        let before = snapshot(&state).await;
        assert_eq!(withdraw(&app, &cookie, id, 3).await.0, StatusCode::CONFLICT);
        assert_eq!(
            decide(&app, &reviewer, id, "approved", 3, &value["difficulties"])
                .await
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            snapshot(&state).await,
            before,
            "closed and no-op operations cannot emit audits"
        );
        let (status, second) = create_local(&app, &cookie, set).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a withdrawn request releases global pending uniqueness: {second}"
        );
        assert_ne!(created(&second, set, "local"), id);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn concurrent_rank_requests_and_reviews_publish_one_atomic_decision() {
    let fixture = MutationFixture::new("rank_req_race", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result=tokio::spawn(async move {
        let (owner,_,_,cookie)=actor(&state,"RankConcurrentOwner",&[REQUEST_PERMISSION]).await;
        let (_,_,_,first)=actor(&state,"RankConcurrentFirst",&[REVIEW_PERMISSION,"beatmaps.moderate"]).await;
        let (_,_,_,second)=actor(&state,"RankConcurrentSecond",&[REVIEW_PERMISSION,"beatmaps.moderate"]).await;
        let set=local_set(&state,owner,&["graveyard","pending"]).await;
        let (left,right)=tokio::join!(create_local(&app,&cookie,set),create_local(&app,&cookie,set));
        let submissions=[left,right];
        assert_eq!(submissions.iter().filter(|(s,_)| *s==StatusCode::OK).count(),1,"{submissions:?}");
        assert_eq!(submissions.iter().filter(|(s,_)| *s==StatusCode::CONFLICT).count(),1,"{submissions:?}");
        let submitted=&submissions.iter().find(|(s,_)| *s==StatusCode::OK).unwrap().1;
        let id=created(submitted,set,"local");
        let (left,right)=tokio::join!(decide(&app,&first,id,"approved",0,&submitted["difficulties"]),decide(&app,&second,id,"rejected",0,&Value::Null));
        let reviews=[left,right];
        assert_eq!(reviews.iter().filter(|(s,_)| *s==StatusCode::OK).count(),1,"{reviews:?}");
        assert_eq!(reviews.iter().filter(|(s,_)| *s==StatusCode::CONFLICT).count(),1,"{reviews:?}");
        let row=stored(&state,id).await;
        assert_eq!(row["revision"],1);
        let audits:i64=sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE target_type='beatmap_rank_request' AND target_id=$1")
            .bind(id.to_string()).fetch_one(&state.db).await.unwrap();
        assert_eq!(audits,2,"one create and one review audit");
        let statuses:Vec<String>=sqlx::query_scalar("SELECT status FROM beatmaps WHERE native_set_id=$1 ORDER BY id")
            .bind(set).fetch_all(&state.db).await.unwrap();
        if row["status"]=="approved" {
            assert_eq!(statuses,vec!["ranked","pending"]);
        } else {
            assert_eq!(row["status"],"rejected");
            assert_eq!(statuses,vec!["graveyard","pending"]);
        }
    }).await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn approval_requires_moderation_and_ranks_only_selected_difficulties() {
    let fixture = MutationFixture::new("rank_req_approve", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (owner, _, _, cookie) = actor(&state, "RankApprovalOwner", &[REQUEST_PERMISSION]).await;
        let (_, _, _, review_only) =
            actor(&state, "RankApprovalReviewOnly", &[REVIEW_PERMISSION]).await;
        let (_, _, _, moderate_only) =
            actor(&state, "RankApprovalModerateOnly", &["beatmaps.moderate"]).await;
        let (_, _, _, staff) = actor(
            &state,
            "RankApprovalStaff",
            &[REVIEW_PERMISSION, "beatmaps.moderate"],
        )
        .await;
        let set = local_set(&state, owner, &["graveyard", "pending", "unranked"]).await;
        let (status, value) = create_local(&app, &cookie, set).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let id = created(&value, set, "local");
        let before = snapshot(&state).await;
        assert_eq!(
            decide(
                &app,
                &review_only,
                id,
                "approved",
                0,
                &value["difficulties"]
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            decide(
                &app,
                &moderate_only,
                id,
                "approved",
                0,
                &value["difficulties"]
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        for input in [
            json!({"decision":"pending","reason":"Review","expected_revision":0}),
            json!({"decision":"approved","reason":"Review"}),
            json!({"decision":"approved","reason":"Review","expected_revision":-1}),
        ] {
            let status = request(&app, "PATCH", &format!("{QUEUE}/{id}"), Some(&staff), input)
                .await
                .0;
            assert!(matches!(
                status,
                StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
            ));
        }
        assert_eq!(snapshot(&state).await, before);
        let selected = value["difficulties"][0]["native_id"].as_i64().unwrap();
        let (status, invalid_reason) = request(
            &app,
            "PATCH",
            &format!("{QUEUE}/{id}"),
            Some(&staff),
            json!({
                "decision":"approved","reason":"unsafe\nreason","expected_revision":0,
                "difficulty_ids":[selected],"difficulty_revisions":[{
                    "native_id":selected,"expected_revision":0,"expected_status_revision":0
                }]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid_reason}");
        assert_eq!(
            invalid_reason["error"]["code"],
            "invalid_rank_request_reason"
        );
        assert_eq!(snapshot(&state).await, before);
        let (status, value) = decide(&app, &staff, id, "approved", 0, &value["difficulties"]).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["status"], "approved");
        assert_eq!(value["revision"], 1);
        assert_eq!(value["approved_difficulty_ids"], json!([selected]));
        assert_public(&value);
        let after = snapshot(&state).await;
        assert_eq!(after["beatmap_packages"], before["beatmap_packages"]);
        for (old, new) in before["beatmaps"]
            .as_array()
            .unwrap()
            .iter()
            .zip(after["beatmaps"].as_array().unwrap())
        {
            let mut original = old.clone();
            if old["native_id"] == selected {
                assert_eq!(new["status"], "ranked");
                assert_eq!(new["status_revision"], 1);
                original["status"] = json!("ranked");
                original["status_revision"] = json!(1);
            }
            assert_eq!(
                *new, original,
                "approval preserves immutable metadata, identifiers and stored paths"
            );
        }
        assert_eq!(
            after["beatmap_status_overrides"].as_array().unwrap().len(),
            1
        );
        assert_eq!(after["beatmap_status_overrides"][0]["native_set_id"], set);
        assert_eq!(after["beatmap_status_overrides"][0]["native_id"], selected);
        assert_eq!(after["beatmap_status_overrides"][0]["status"], "ranked");
        let reject_set = local_set(&state, owner, &["graveyard"]).await;
        let (_, value) = create_local(&app, &cookie, reject_set).await;
        let reject_id = created(&value, reject_set, "local");
        assert_eq!(
            decide(&app, &review_only, reject_id, "rejected", 0, &Value::Null)
                .await
                .0,
            StatusCode::OK,
            "review alone can reject without publishing ranked maps"
        );
    })
    .await;
    fixture.finish(result).await;
}

fn official_metadata(set: i64, status: &str) -> Value {
    json!({"id":set,"title":"Official rank fixture","artist":"Fixture Artist","creator":"Fixture Creator","status":status,"beatmaps":[
        {"id":set*10,"beatmapset_id":set,"checksum":format!("{:032x}",set*10),"version":"Easy","mode_int":0,"status":status,"difficulty_rating":1.0,"total_length":120},
        {"id":set*10+1,"beatmapset_id":set,"checksum":format!("{:032x}",set*10+1),"version":"Hard","mode_int":0,"status":status,"difficulty_rating":4.0,"total_length":120}
    ]})
}

async fn create_official(app: &Router, cookie: &str, set: i64) -> (StatusCode, Value) {
    request(
        app,
        "POST",
        CREATE,
        Some(cookie),
        json!({"official_set_id":set,"reason":"  Please review this set  "}),
    )
    .await
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn official_nominations_are_global_metadata_only_and_future_imports_keep_selected_status() {
    let server = OfficialServer::start().await;
    server.add_set(42, official_metadata(42, "graveyard")).await;
    server.add_set(43, official_metadata(43, "ranked")).await;
    let fixture = MutationFixture::new("rank_req_official", server.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result=tokio::spawn(async move {
        let (_,_,_,first)=actor(&state,"RankOfficialFirst",&[REQUEST_PERMISSION]).await;
        let (_,_,_,second)=actor(&state,"RankOfficialSecond",&[REQUEST_PERMISSION]).await;
        let (staff_id,_,_,staff)=actor(&state,"RankOfficialStaff",&[REVIEW_PERMISSION,"beatmaps.moderate"]).await;
        let before=snapshot(&state).await;
        assert_eq!(create_official(&app,&first,43).await.0,StatusCode::BAD_REQUEST);
        let (status,value)=create_official(&app,&first,999).await;
        assert_ne!(status,StatusCode::OK,"missing upstream metadata cannot create a request: {value}");
        assert_eq!(snapshot(&state).await,before);
        let (left,right)=tokio::join!(create_official(&app,&first,42),create_official(&app,&second,42));
        let results=[left,right];
        assert_eq!(results.iter().filter(|(s,_)| *s==StatusCode::OK).count(),1,"{results:?}");
        assert_eq!(results.iter().filter(|(s,_)| *s==StatusCode::CONFLICT).count(),1,"uniqueness is global across different players: {results:?}");
        let value=&results.iter().find(|(s,_)| *s==StatusCode::OK).unwrap().1;
        let id=created(value,42,"official");
        assert_eq!(value["metadata"]["official_url"],"https://osu.ppy.sh/beatmapsets/42");
        assert_eq!(value["metadata"]["difficulty_count"],2);
        assert_eq!(value["difficulties"][0]["status_revision"],Value::Null);
        let (status,value)=decide(&app,&staff,id,"approved",0,&value["difficulties"]).await;
        assert_eq!(status,StatusCode::OK,"{value}");
        assert_eq!(value["approved_difficulty_ids"],json!([420]));
        for table in ["beatmaps","beatmap_packages","official_beatmapset_archives","official_beatmapset_archive_members"] {
            let count:i64=sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&state.db).await.unwrap();
            assert_eq!(count,0,"nomination and ranking must not invent downloads or import content into {table}");
        }
        assert_eq!(file_count(&state.maps_root).await,0);
        // Model a later legitimate content-cache write with the original osu!
        // metadata. Only the selected native difficulty inherits the decision.
        for native in [420,421] {
            sqlx::query("INSERT INTO beatmaps(official_id,official_set_id,uploaded_by,title,artist,creator,version,mode,status,source,stars,md5,osu_path) VALUES($1,42,$2,'Official rank fixture','Fixture Artist','Fixture Creator','Fixture',0,'graveyard','official',1,$3,$4)")
                .bind(native).bind(staff_id).bind(format!("{native:032x}"))
                .bind(format!("private-future-import-{native}.osu")).execute(&state.db).await.unwrap();
        }
        let maps:Vec<(i64,String,i64)>=sqlx::query_as("SELECT native_id,status,status_revision FROM beatmaps ORDER BY native_id")
            .fetch_all(&state.db).await.unwrap();
        assert_eq!(maps,vec![(420,"ranked".into(),0),(421,"graveyard".into(),0)]);
        sqlx::query("UPDATE beatmaps SET status='pending',status_revision=0 WHERE native_id=420")
            .execute(&state.db).await.unwrap();
        let (status,revision):(String,i64)=sqlx::query_as("SELECT status,status_revision FROM beatmaps WHERE native_id=420")
            .fetch_one(&state.db).await.unwrap();
        assert_eq!((status,revision),("ranked".into(),0),"upstream refresh cannot erase the local decision");
        assert_eq!(server.counters()[3],0,"no raw .osu request is needed");
        assert!(server.set_queries().await.contains(&42));
    }).await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn withdrawn_official_staff_snapshot_restores_without_public_or_status_cas_bypass() {
    let server = OfficialServer::start().await;
    server.add_set(42, official_metadata(42, "graveyard")).await;
    let fixture = MutationFixture::new("rank_official_restore", server.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (_, _, _, staff) = actor(&state, "OfficialRestoreBn", &["beatmaps.moderate"]).await;
        let (_, _, _, denied) = actor(&state, "OfficialRestoreDenied", &["admin.view"]).await;
        let staff_path = "/api/v1/admin/beatmapsets/42";
        let public_path = "/api/v1/official/beatmapsets/42";
        let availability_path = "/api/v1/admin/beatmapsets/42/availability";
        assert_eq!(
            request(&app, "GET", public_path, None, Value::Null).await.0,
            StatusCode::OK
        );
        let (status, value) = request(
            &app,
            "PATCH",
            availability_path,
            Some(&staff),
            json!({"withdrawn":true,"expected_revision":0,"reason":"Official fixture withdrawal"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let before = snapshot(&state).await;
        assert_eq!(
            request(&app, "GET", staff_path, None, Value::Null).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(&app, "GET", staff_path, Some(&denied), Value::Null)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let (status, value) = request(&app, "GET", public_path, None, Value::Null).await;
        assert_eq!(
            status,
            StatusCode::GONE,
            "a warm official cache cannot disclose a withdrawn set: {value}"
        );
        let (status, view) = request(&app, "GET", staff_path, Some(&staff), Value::Null).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "moderate-only BN can inspect the withdrawn official set: {view}"
        );
        assert_eq!(view["source"], "official");
        assert_eq!(view["native_set_id"], 42);
        assert_eq!(view["availability"]["withdrawn"], true);
        assert_eq!(view["availability"]["revision"], 1);
        assert_eq!(
            view["availability"]["reason"],
            "Official fixture withdrawal"
        );
        assert!(view["availability"].get("actor_id").is_none());
        assert_eq!(view["representative_id"], Value::Null);
        assert_eq!(view["difficulties"].as_array().unwrap().len(), 2);
        assert_eq!(view["difficulties"][0]["native_id"], 420);
        assert_eq!(view["difficulties"][0]["revision"], 0);
        assert_eq!(view["difficulties"][0]["status_revision"], Value::Null);
        assert_public(&view);
        assert_eq!(
            snapshot(&state).await,
            before,
            "inspection must not import official maps or change moderation state"
        );
        let (status, value) = difficulty_patch(&app, &staff, 42, 420, "ranked", 0, None).await;
        assert_eq!(
            status,
            StatusCode::GONE,
            "verified staff membership cannot bypass withdrawal for a status edit: {value}"
        );
        let (status, value) = request(
            &app,
            "PATCH",
            availability_path,
            Some(&staff),
            json!({"withdrawn":false,"expected_revision":0,"reason":"Ready for moderation"}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "stale restoration CAS: {value}"
        );
        assert_eq!(snapshot(&state).await, before);
        let (status, value) = request(
            &app,
            "PATCH",
            availability_path,
            Some(&staff),
            json!({"withdrawn":false,"expected_revision":1,"reason":"Ready for moderation"}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "existing availability API restores with beatmaps.moderate alone: {value}"
        );
        assert_eq!(value["availability"]["withdrawn"], false);
        assert_eq!(value["availability"]["revision"], 2);
        let (status, view) = request(&app, "GET", staff_path, Some(&staff), Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{view}");
        assert_eq!(view["availability"]["withdrawn"], false);
        assert_eq!(
            view["availability"]["revision"], 2,
            "availability is read afresh despite the upstream cache"
        );
        assert_public(&view);
        assert_eq!(
            request(&app, "GET", public_path, None, Value::Null).await.0,
            StatusCode::OK
        );
        let (status, value) = difficulty_patch(&app, &staff, 42, 420, "ranked", 0, None).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "pre-import null status revision remains a valid CAS after restore: {value}"
        );
        assert_eq!(value["difficulties"][0]["status"], "ranked");
        assert_eq!(value["difficulties"][0]["revision"], 1);
        assert_eq!(value["difficulties"][0]["status_revision"], Value::Null);
        assert_eq!(value["difficulties"][1]["status"], "graveyard");
        let before = snapshot(&state).await;
        let (status, value) = difficulty_patch(&app, &staff, 42, 420, "loved", 0, None).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "stale override CAS cannot publish: {value}"
        );
        assert_eq!(value["error"]["code"], "beatmap_status_conflict");
        assert_eq!(snapshot(&state).await, before);
        let (status, value) = request(&app, "GET", public_path, None, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["difficulties"][0]["status"], "ranked");
        assert_eq!(value["difficulties"][1]["status"], "graveyard");
        assert_eq!(before["beatmaps"], json!([]));
        assert_eq!(before["beatmap_packages"], json!([]));
        assert_eq!(file_count(&state.maps_root).await, 0);
        assert_eq!(
            server.counters()[3],
            0,
            "staff metadata never requests raw .osu content"
        );
        assert_eq!(
            server.set_queries().await,
            vec![42],
            "staff and public detail share the bounded verified upstream cache"
        );
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn withdrawn_official_staff_snapshot_rechecks_upstream_wait_and_projection_deadline() {
    let server = OfficialServer::start().await;
    for id in 51..=54 {
        server.add_set(id, official_metadata(id, "graveyard")).await;
    }
    let fixture = MutationFixture::new("rank_official_auth", server.origin.clone()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let application = fixture.schema.clone();
    let result = tokio::spawn(async move {
        for (index, change) in ["permission", "session", "expiry"].into_iter().enumerate() {
            let set = 51 + index as i64;
            let (user, role, session, cookie) = actor(&state, &format!("OfficialReadAuth{index}"), &["beatmaps.moderate"]).await;
            sqlx::query("INSERT INTO beatmapset_availability(native_set_id,withdrawn,revision,reason,actor_id) VALUES($1,true,1,'Withdrawn auth fixture',$2)")
                .bind(set).bind(user).execute(&state.db).await.unwrap();
            let expiry = if change == "expiry" {
                Some(sqlx::query_scalar::<_, DateTime<Utc>>("UPDATE sessions SET expires_at=clock_timestamp()+interval '1500 milliseconds' WHERE id=$1 RETURNING expires_at")
                    .bind(session).fetch_one(&state.db).await.unwrap())
            } else { None };
            let before = snapshot(&state).await;
            let pause = server.pause_next_set().await;
            let queued_app = app.clone();
            let queued = tokio::spawn(async move { request(&queued_app, "GET", &format!("/api/v1/admin/beatmapsets/{set}"), Some(&cookie), Value::Null).await });
            pause.wait_entered().await;
            match change {
                "permission" => { sqlx::query("UPDATE roles SET permissions='{}' WHERE id=$1").bind(role).execute(&state.db).await.unwrap(); }
                "session" => { sqlx::query("DELETE FROM sessions WHERE id=$1").bind(session).execute(&state.db).await.unwrap(); }
                "expiry" => { wait_until_expired(&state.db, expiry.unwrap()).await; }
                _ => unreachable!(),
            }
            pause.release();
            let (status, value) = queued.await.unwrap();
            assert_eq!(status, if change == "permission" { StatusCode::FORBIDDEN } else { StatusCode::UNAUTHORIZED }, "{change} during upstream work must suppress the staff DTO: {value}");
            assert_eq!(snapshot(&state).await, before);
        }
        let (user, _, session, cookie) = actor(&state, "OfficialReadProjectionExpiry", &["beatmaps.moderate"]).await;
        sqlx::query("INSERT INTO beatmapset_availability(native_set_id,withdrawn,revision,reason,actor_id) VALUES(54,true,1,'Projection deadline fixture',$1)")
            .bind(user).execute(&state.db).await.unwrap();
        let path = "/api/v1/admin/beatmapsets/54";
        assert_eq!(request(&app, "GET", path, Some(&cookie), Value::Null).await.0, StatusCode::OK);
        let expiry: DateTime<Utc> = sqlx::query_scalar("UPDATE sessions SET expires_at=clock_timestamp()+interval '3 seconds' WHERE id=$1 RETURNING expires_at")
            .bind(session).fetch_one(&state.db).await.unwrap();
        let before = snapshot(&state).await;
        let mut blocker = state.db.begin().await.unwrap();
        sqlx::query("LOCK TABLE beatmapset_availability IN ACCESS EXCLUSIVE MODE").execute(&mut *blocker).await.unwrap();
        let queued_app = app.clone();
        let queued = tokio::spawn(async move { request(&queued_app, "GET", path, Some(&cookie), Value::Null).await });
        wait_for_lock(&state.db, &application, "beatmapset_availability WHERE native_set_id").await;
        wait_until_expired(&state.db, expiry).await;
        blocker.commit().await.unwrap();
        let (status, value) = queued.await.unwrap();
        assert_eq!(status, StatusCode::UNAUTHORIZED, "actor expiry during SQL projection must suppress the staff DTO: {value}");
        assert_eq!(snapshot(&state).await, before);
    }).await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn nomination_limits_count_open_requests_and_closed_daily_history() {
    let fixture = MutationFixture::new("rank_req_limits", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (owner, role, _, cookie) = actor(&state, "RankLimitOpen", &[REQUEST_PERMISSION]).await;
        let sets = local_set(&state, owner, &["graveyard"]).await;
        let (status, value) = create_local(&app, &cookie, sets).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let first = created(&value, sets, "local");
        for _ in 1..5 {
            let set = local_set(&state, owner, &["graveyard"]).await;
            let (status, value) = create_local(&app, &cookie, set).await;
            assert_eq!(status, StatusCode::OK, "{value}");
        }
        let sixth = local_set(&state, owner, &["graveyard"]).await;
        let before = snapshot(&state).await;
        let (status, value) = create_local(&app, &cookie, sixth).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{value}");
        assert_eq!(value["error"]["code"], "rank_request_limit");
        assert_eq!(snapshot(&state).await, before);
        // Revoking admission authority still permits the owner to close their
        // existing nomination; the open limit must not trap the player.
        sqlx::query("UPDATE roles SET permissions='{}' WHERE id=$1")
            .bind(role)
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(withdraw(&app, &cookie, first, 0).await.0, StatusCode::OK);
        assert_eq!(
            create_local(&app, &cookie, sixth).await.0,
            StatusCode::FORBIDDEN
        );
        sqlx::query("UPDATE roles SET permissions=$2 WHERE id=$1")
            .bind(role)
            .bind(vec![REQUEST_PERMISSION])
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(create_local(&app, &cookie, sixth).await.0, StatusCode::OK);
        let (daily_owner, _, _, daily) =
            actor(&state, "RankLimitDaily", &[REQUEST_PERMISSION]).await;
        for _ in 0..10 {
            let set = local_set(&state, daily_owner, &["graveyard"]).await;
            let (status, value) = create_local(&app, &daily, set).await;
            assert_eq!(status, StatusCode::OK, "{value}");
            let id = created(&value, set, "local");
            assert_eq!(withdraw(&app, &daily, id, 0).await.0, StatusCode::OK);
        }
        let extra = local_set(&state, daily_owner, &["graveyard"]).await;
        let before = snapshot(&state).await;
        let (status, value) = create_local(&app, &daily, extra).await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "closed requests still count toward the 24-hour limit: {value}"
        );
        assert_eq!(value["error"]["code"], "rank_request_limit");
        assert_eq!(snapshot(&state).await, before);
    })
    .await;
    fixture.finish(result).await;
}

async fn difficulty_patch(
    app: &Router,
    cookie: &str,
    set: i64,
    native: i64,
    status: &str,
    revision: i64,
    map_revision: Option<i64>,
) -> (StatusCode, Value) {
    request(app,"PATCH",&format!("/api/v1/admin/beatmapsets/{set}/difficulties/{native}"),Some(cookie),json!({"status":status,"reason":"Difficulty review","expected_revision":revision,"expected_status_revision":map_revision})).await
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn difficulty_status_cas_tracks_aba_and_never_changes_siblings() {
    let fixture = MutationFixture::new("rank_difficulty_cas", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result=tokio::spawn(async move {
        let (owner,_,_,_)=actor(&state,"DifficultyCasOwner",&[REQUEST_PERMISSION]).await;
        let (_,_,_,staff)=actor(&state,"DifficultyCasStaff",&["beatmaps.moderate"]).await;
        let (_,_,_,denied)=actor(&state,"DifficultyCasDenied",&[REVIEW_PERMISSION,"admin.view"]).await;
        let set=local_set(&state,owner,&["graveyard","pending"]).await;
        let maps:Vec<i64>=sqlx::query_scalar("SELECT native_id FROM beatmaps WHERE native_set_id=$1 ORDER BY id")
            .bind(set).fetch_all(&state.db).await.unwrap();
        let native=maps[0];
        let before=snapshot(&state).await;
        assert_eq!(difficulty_patch(&app,&denied,set,native,"ranked",0,Some(0)).await.0,StatusCode::FORBIDDEN);
        assert_eq!(difficulty_patch(&app,&staff,set,native,"ranked",0,None).await.0,StatusCode::CONFLICT,"an imported map requires its concrete status revision");
        assert_eq!(snapshot(&state).await,before);
        for (target,revision) in [("ranked",0),("graveyard",1),("ranked",2)] {
            let (status,value)=difficulty_patch(&app,&staff,set,native,target,revision,Some(revision)).await;
            assert_eq!(status,StatusCode::OK,"{value}");
        }
        let before=snapshot(&state).await;
        let (status,value)=difficulty_patch(&app,&staff,set,native,"loved",1,Some(1)).await;
        assert_eq!(status,StatusCode::CONFLICT,"ABA cannot make an old status editor fresh: {value}");
        assert_eq!(value["error"]["code"],"beatmap_status_conflict");
        assert_eq!(snapshot(&state).await,before);
        let override_row:Value=sqlx::query_scalar("SELECT to_jsonb(o) FROM beatmap_status_overrides o WHERE native_id=$1")
            .bind(native).fetch_one(&state.db).await.unwrap();
        assert_eq!(override_row["revision"],3);
        assert_eq!(override_row["status"],"ranked");
        let states:Vec<(i64,String,i64)>=sqlx::query_as("SELECT native_id,status,status_revision FROM beatmaps WHERE native_set_id=$1 ORDER BY id")
            .bind(set).fetch_all(&state.db).await.unwrap();
        assert_eq!(states,vec![(native,"ranked".into(),3),(maps[1],"pending".into(),0)]);
        sqlx::query("UPDATE beatmaps SET stars=5,status_revision=0 WHERE native_id=$1")
            .bind(native).execute(&state.db).await.unwrap();
        let revision:i64=sqlx::query_scalar("SELECT status_revision FROM beatmaps WHERE native_id=$1")
            .bind(native).fetch_one(&state.db).await.unwrap();
        assert_eq!(revision,3,"unrelated metadata refresh preserves the status revision");
        let other=local_set(&state,owner,&["graveyard"]).await;
        let before=snapshot(&state).await;
        assert_eq!(difficulty_patch(&app,&staff,other,native,"loved",3,Some(3)).await.0,StatusCode::NOT_FOUND,"native ID belongs to one verified set context");
        assert_eq!(snapshot(&state).await,before);
        let internal:i64=sqlx::query_scalar("SELECT id FROM beatmaps WHERE native_id=$1").bind(native).fetch_one(&state.db).await.unwrap();
        let legacy=format!("/api/v1/admin/beatmaps/{internal}");
        assert_eq!(request(&app,"PATCH",&legacy,Some(&staff),json!({"status":"loved"})).await.0,StatusCode::UNPROCESSABLE_ENTITY,"the old route cannot bypass status revisions");
        let (status,value)=request(&app,"PATCH",&legacy,Some(&staff),json!({"status":"loved","expected_revision":1,"expected_status_revision":1})).await;
        assert_eq!(status,StatusCode::CONFLICT,"stale legacy editor: {value}");
        assert_eq!(value["error"]["code"],"beatmap_status_conflict");
        assert_eq!(snapshot(&state).await,before);
        let (status,value)=request(&app,"PATCH",&legacy,Some(&staff),json!({"status":"loved","expected_revision":3,"expected_status_revision":3})).await;
        assert_eq!(status,StatusCode::OK,"fresh legacy editor: {value}");
        assert_eq!(value["status"],"loved");
        assert_eq!(value["status_override_revision"],4);
        assert_eq!(value["status_revision"],4);
        let sibling:String=sqlx::query_scalar("SELECT status FROM beatmaps WHERE native_id=$1").bind(maps[1]).fetch_one(&state.db).await.unwrap();
        assert_eq!(sibling,"pending");
    }).await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn queued_rank_review_rechecks_role_loss_and_actual_session_expiry() {
    let fixture = MutationFixture::new("rank_req_auth", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let application = fixture.schema.clone();
    let result=tokio::spawn(async move {
        let (owner,_,_,cookie)=actor(&state,"RankQueueOwner",&[REQUEST_PERMISSION]).await;
        let set=local_set(&state,owner,&["graveyard"]).await;
        let (status,value)=create_local(&app,&cookie,set).await;
        assert_eq!(status,StatusCode::OK,"{value}");
        let id=created(&value,set,"local");
        for (index,change) in ["permission","session","restriction"].into_iter().enumerate() {
            let (user,role,sid,cookie)=actor(&state,&format!("RankQueuedReviewer{index}"),&[REVIEW_PERMISSION]).await;
            let before=snapshot(&state).await;
            let mut blocker=state.db.begin().await.unwrap();
            sqlx::query("LOCK TABLE roles,user_roles IN SHARE ROW EXCLUSIVE MODE")
                .execute(&mut *blocker).await.unwrap();
            let queued_app=app.clone();
            let queued=tokio::spawn(async move {decide(&queued_app,&cookie,id,"rejected",0,&Value::Null).await});
            wait_for_lock(&state.db,&application,"LOCK TABLE roles,user_roles IN SHARE MODE").await;
            match change {
                "permission" => {sqlx::query("UPDATE roles SET permissions='{}' WHERE id=$1").bind(role).execute(&mut *blocker).await.unwrap();}
                "session" => {sqlx::query("DELETE FROM sessions WHERE id=$1").bind(sid).execute(&mut *blocker).await.unwrap();}
                "restriction" => {sqlx::query("UPDATE users SET is_restricted=true WHERE id=$1").bind(user).execute(&mut *blocker).await.unwrap();}
                _ => unreachable!(),
            }
            blocker.commit().await.unwrap();
            let (status,value)=tokio::time::timeout(Duration::from_secs(5),queued).await.unwrap().unwrap();
            assert_eq!(status,if change=="permission" {StatusCode::FORBIDDEN} else {StatusCode::UNAUTHORIZED},"{change}: {value}");
            assert_eq!(snapshot(&state).await,before);
        }
        let (_,_,sid,cookie)=actor(&state,"RankExpiredReviewer",&[REVIEW_PERMISSION]).await;
        let expiry:DateTime<Utc>=sqlx::query_scalar("UPDATE sessions SET expires_at=clock_timestamp()+interval '1500 milliseconds' WHERE id=$1 RETURNING expires_at")
            .bind(sid).fetch_one(&state.db).await.unwrap();
        let before=snapshot(&state).await;
        let mut blocker=state.db.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock($1,$2)").bind(1229737811_i32).bind(set as i32)
            .execute(&mut *blocker).await.unwrap();
        let queued_app=app.clone();
        let queued=tokio::spawn(async move {decide(&queued_app,&cookie,id,"rejected",0,&Value::Null).await});
        wait_for_lock(&state.db,&application,"SELECT pg_advisory_xact_lock($1,$2)").await;
        wait_until_expired(&state.db,expiry).await;
        blocker.commit().await.unwrap();
        let (status,value)=queued.await.unwrap();
        assert_eq!(status,StatusCode::UNAUTHORIZED,"actual database session expiry is rechecked after the set wait: {value}");
        assert_eq!(snapshot(&state).await,before);
    }).await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn rank_request_audit_failure_rolls_back_create_withdraw_and_selected_approval() {
    for operation in ["create", "withdraw", "approve"] {
        let fixture =
            MutationFixture::new(&format!("rank_rb_{operation}"), "http://127.0.0.1:1".into())
                .await;
        let state = fixture.state.clone();
        let app = fixture.app.clone();
        let result=tokio::spawn(async move {
            let (owner,_,_,cookie)=actor(&state,"RankRollbackOwner",&[REQUEST_PERMISSION]).await;
            let (_,_,_,staff)=actor(&state,"RankRollbackStaff",&[REVIEW_PERMISSION,"beatmaps.moderate"]).await;
            let set=local_set(&state,owner,&["graveyard","pending"]).await;
            let submitted=if operation=="create" {Value::Null} else {
                let (status,value)=create_local(&app,&cookie,set).await;
                assert_eq!(status,StatusCode::OK,"{value}");
                value
            };
            reject_audits_at_commit(&state.db).await;
            let before=snapshot(&state).await;
            let (status,value)=match operation {
                "create" => create_local(&app,&cookie,set).await,
                "withdraw" => withdraw(&app,&cookie,submitted["id"].as_i64().unwrap(),0).await,
                "approve" => decide(&app,&staff,submitted["id"].as_i64().unwrap(),"approved",0,&submitted["difficulties"]).await,
                _ => unreachable!(),
            };
            assert_eq!(status,StatusCode::INTERNAL_SERVER_ERROR,"{operation}: {value}");
            assert_eq!(snapshot(&state).await,before,"{operation} cannot commit policy, request revisions or notifications without its audit");
            assert_eq!(file_count(&state.maps_root).await,0);
        }).await;
        fixture.finish(result).await;
    }
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn approval_subset_validation_and_one_stale_difficulty_prevent_partial_publication() {
    let fixture = MutationFixture::new("rank_req_subset", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result=tokio::spawn(async move {
        let (owner,_,_,cookie)=actor(&state,"RankSubsetOwner",&[REQUEST_PERMISSION]).await;
        let (_,_,_,staff)=actor(&state,"RankSubsetStaff",&[REVIEW_PERMISSION,"beatmaps.moderate"]).await;
        let set=local_set(&state,owner,&["graveyard","pending","unranked"]).await;
        let natives:Vec<i64>=sqlx::query_scalar("SELECT native_id FROM beatmaps WHERE native_set_id=$1 ORDER BY id")
            .bind(set).fetch_all(&state.db).await.unwrap();
        let before=snapshot(&state).await;
        for ids in [vec![],vec![natives[0],natives[0]],vec![1],vec![natives[0],1]] {
            let (status,value)=request(&app,"POST",CREATE,Some(&cookie),json!({"native_set_id":set,"difficulty_ids":ids,"reason":"Review"})).await;
            assert_eq!(status,StatusCode::BAD_REQUEST,"invalid nomination subset: {value}");
            assert_eq!(snapshot(&state).await,before);
        }
        let (status,submitted)=request(&app,"POST",CREATE,Some(&cookie),json!({"native_set_id":set,"difficulty_ids":[natives[0],natives[1]],"reason":"Please review this set"})).await;
        assert_eq!(status,StatusCode::OK,"{submitted}");
        let id=created(&submitted,set,"local");
        assert_eq!(submitted["difficulties"].as_array().unwrap().len(),2);
        let before=snapshot(&state).await;
        let valid=json!({"decision":"approved","expected_revision":0,"reason":"Review complete","difficulty_ids":[natives[0],natives[1]],"difficulty_revisions":[
            {"native_id":natives[0],"expected_revision":0,"expected_status_revision":0},
            {"native_id":natives[1],"expected_revision":0,"expected_status_revision":0}
        ]});
        for ids in [vec![],vec![natives[0],natives[0]],vec![natives[2]],vec![natives[0],natives[2]]] {
            let mut input=valid.clone();
            input["difficulty_revisions"]=json!(ids.iter().map(|native|json!({"native_id":native,"expected_revision":0,"expected_status_revision":0})).collect::<Vec<_>>());
            input["difficulty_ids"]=json!(ids);
            let (status,value)=request(&app,"PATCH",&format!("{QUEUE}/{id}"),Some(&staff),input).await;
            assert_eq!(status,StatusCode::BAD_REQUEST,"only nominated difficulties can be approved: {value}");
            assert_eq!(snapshot(&state).await,before);
        }
        let mut stale=valid.clone();
        stale["difficulty_revisions"][1]["expected_status_revision"]=json!(1);
        let (status,value)=request(&app,"PATCH",&format!("{QUEUE}/{id}"),Some(&staff),stale).await;
        assert_eq!(status,StatusCode::CONFLICT,"the second stale difficulty rolls back a tentative first decision: {value}");
        assert_eq!(value["error"]["code"],"beatmap_status_conflict");
        assert_eq!(snapshot(&state).await,before);
        let (status,value)=request(&app,"PATCH",&format!("{QUEUE}/{id}"),Some(&staff),valid).await;
        assert_eq!(status,StatusCode::OK,"{value}");
        assert_eq!(value["approved_difficulty_ids"],json!([natives[0],natives[1]]));
        let statuses:Vec<String>=sqlx::query_scalar("SELECT status FROM beatmaps WHERE native_set_id=$1 ORDER BY id")
            .bind(set).fetch_all(&state.db).await.unwrap();
        assert_eq!(statuses,vec!["ranked","ranked","unranked"]);
        let unavailable=local_set(&state,owner,&["graveyard"]).await;
        let (status,submitted)=create_local(&app,&cookie,unavailable).await;
        assert_eq!(status,StatusCode::OK,"{submitted}");
        let unavailable_id=created(&submitted,unavailable,"local");
        sqlx::query("INSERT INTO beatmapset_availability(native_set_id,withdrawn,revision,reason,actor_id) VALUES($1,true,1,'Fixture withdrawal',$2)")
            .bind(unavailable).bind(owner).execute(&state.db).await.unwrap();
        let before=snapshot(&state).await;
        let (status,value)=decide(&app,&staff,unavailable_id,"approved",0,&submitted["difficulties"]).await;
        assert_eq!(status,StatusCode::GONE,"a pending nomination cannot bypass an availability withdrawal: {value}");
        assert_eq!(snapshot(&state).await,before);
        let (status,view)=request(&app,"GET",&format!("/api/v1/admin/beatmapsets/{unavailable}"),Some(&staff),Value::Null).await;
        assert_eq!(status,StatusCode::OK,"a reviewer without admin.view can inspect the withdrawn set: {view}");
        assert_eq!(view["availability"]["withdrawn"],true);
        assert_eq!(view["availability"]["revision"],1);
        assert_eq!(view["availability"]["reason"],"Fixture withdrawal");
        assert!(view["representative_id"].as_i64().is_some());
        assert!(view["availability"].get("actor_id").is_none());
        assert_public(&view);
        let availability_path=format!("/api/v1/admin/beatmapsets/{unavailable}/availability");
        let (status,value)=request(&app,"PATCH",&availability_path,Some(&staff),json!({"withdrawn":false,"expected_revision":0,"reason":"Ready for moderation"})).await;
        assert_eq!(status,StatusCode::CONFLICT,"a stale restore cannot overwrite an availability decision: {value}");
        assert_eq!(snapshot(&state).await,before);
        let (status,value)=request(&app,"PATCH",&availability_path,Some(&staff),json!({"withdrawn":false,"expected_revision":1,"reason":"Ready for moderation"})).await;
        assert_eq!(status,StatusCode::OK,"the existing restore route only needs beatmaps.moderate: {value}");
        assert_eq!(value["availability"]["withdrawn"],false);
        assert_eq!(value["availability"]["revision"],2);
        let native=view["difficulties"][0]["native_id"].as_i64().unwrap();
        let (status,value)=difficulty_patch(&app,&staff,unavailable,native,"pending",0,Some(0)).await;
        assert_eq!(status,StatusCode::OK,"per-difficulty status editing resumes after restore: {value}");
        assert_eq!(value["difficulties"][0]["status"],"pending");
        assert_eq!(value["availability"]["withdrawn"],false);
        assert_eq!(value["availability"]["revision"],2);
    }).await;
    fixture.finish(result).await;
}
