//! HTTP moderation and queued-write proofs use only fresh owned PG17 schemas.
use super::*;
use crate::maps::map_mutation_tests::{
    MutationFixture, actor, reject_audits_at_commit, wait_for_lock, wait_until_expired,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, header},
};
use chrono::Duration;
use std::time::Duration as StdDuration;
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

fn silence(expiry: DateTime<Utc>, revision: i64) -> Value {
    json!({"expires_at":expiry,"reason":" Fixture reason ","expected_revision":revision})
}

async fn audit_count(db: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(db)
        .await
        .unwrap()
}

async fn revision(db: &sqlx::PgPool, id: i64) -> i64 {
    sqlx::query_scalar("SELECT chat_silence_revision FROM users WHERE id=$1")
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn message_count(db: &sqlx::PgPool) -> (i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM chat_messages),(SELECT count(*) FROM protocol_private_messages)")
        .fetch_one(db).await.unwrap()
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn timed_chat_moderation_privacy_authority_audit_and_notifications() {
    let fixture = MutationFixture::new("chat_silence_policy", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let outcome = tokio::spawn(async move {
        let (moderator, _, _, mod_cookie) =
            actor(&state, "ChatOnlyModerator", &["chat.moderate"]).await;
        let (player, _, _, player_cookie) = actor(&state, "SilencedPlayer", &["chat.send"]).await;
        let (peer, _, _, peer_cookie) = actor(&state, "PeerModerator", &["chat.moderate"]).await;
        let (administrator, _, _, admin_cookie) = actor(&state, "FullAdministrator", &["*"]).await;
        let (outsider, _, _, outsider_cookie) =
            actor(&state, "OutsideModerator", &["users.moderate"]).await;
        let expiry = Utc::now() + Duration::hours(1);
        let path = format!("/api/v1/admin/users/{player}/chat-silence");
        let before = audit_count(&state.db).await;
        assert_eq!(
            request(
                &app,
                Method::GET,
                "/api/v1/admin/chat-silences",
                &outsider_cookie,
                None
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(&app, Method::GET, "/api/v1/admin/users", &mod_cookie, None)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(
                &app,
                Method::PUT,
                &path,
                &outsider_cookie,
                Some(silence(expiry, 0))
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(
                &app,
                Method::PUT,
                &format!("/api/v1/admin/users/{moderator}/chat-silence"),
                &mod_cookie,
                Some(silence(expiry, 0))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        for protected in [peer, administrator] {
            let (code, value) = request(
                &app,
                Method::PUT,
                &format!("/api/v1/admin/users/{protected}/chat-silence"),
                &mod_cookie,
                Some(silence(expiry, 0)),
            )
            .await;
            assert_eq!(code, StatusCode::FORBIDDEN, "{value}");
            assert_eq!(value["error"]["code"], "protected_chat_operator");
        }
        assert_eq!(audit_count(&state.db).await, before);
        let (_, list) = request(
            &app,
            Method::GET,
            "/api/v1/admin/chat-silences?page_size=100",
            &mod_cookie,
            None,
        )
        .await;
        for item in list["items"].as_array().unwrap() {
            let fields = item.as_object().unwrap();
            assert_eq!(
                fields.len(),
                8,
                "directory must expose only its minimal DTO: {item}"
            );
            for secret in [
                "email",
                "permissions",
                "reason",
                "password_hash",
                "password_md5_hash",
            ] {
                assert!(item.get(secret).is_none());
            }
            let id = item["id"].as_i64().unwrap();
            assert_eq!(
                item["can_silence"],
                json!(![moderator, peer, administrator].contains(&id))
            );
        }
        let (_, exact) = request(
            &app,
            Method::GET,
            &format!("/api/v1/admin/chat-silences?search={player}"),
            &mod_cookie,
            None,
        )
        .await;
        assert_eq!(exact["total"], 1);
        assert_eq!(exact["items"][0]["id"], player);
        let (_, named) = request(
            &app,
            Method::GET,
            "/api/v1/admin/chat-silences?search=silenced",
            &mod_cookie,
            None,
        )
        .await;
        assert_eq!(named["total"], 1);
        assert_eq!(named["items"][0]["id"], player);
        let mut listener = sqlx::postgres::PgListener::connect_with(&state.db)
            .await
            .unwrap();
        listener.listen("illya_chat").await.unwrap();
        let (code, value) = request(
            &app,
            Method::PUT,
            &path,
            &mod_cookie,
            Some(silence(expiry, 0)),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{value}");
        assert_eq!(value["chat_silence_revision"], 1);
        assert!(value["chat_silenced_until"].is_string());
        let notification = tokio::time::timeout(StdDuration::from_secs(3), listener.recv())
            .await
            .unwrap()
            .unwrap();
        let payload: Value = serde_json::from_str(notification.payload()).unwrap();
        assert_eq!(payload["kind"], "chat_policy_changed");
        assert_eq!(payload["user"], player);
        assert_eq!(
            payload.as_object().unwrap().len(),
            3,
            "only schema and identity may enter refresh notifications"
        );
        assert_eq!(audit_count(&state.db).await, before + 1);
        let details: Value = sqlx::query_scalar(
            "SELECT details FROM audit_events WHERE action='chat.silence' AND target_id=$1",
        )
        .bind(player.to_string())
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(details["reason"], "Fixture reason");
        assert_eq!(
            details["previous"],
            json!({"chat_silenced_until":null,"chat_silence_revision":0})
        );
        assert_eq!(details["new"]["chat_silence_revision"], 1);
        let (_, status) = request(
            &app,
            Method::GET,
            "/api/v1/chat/status",
            &player_cookie,
            None,
        )
        .await;
        assert_eq!(status["can_send"], false);
        assert_eq!(status["send_restriction"], "silenced");
        assert!(status["silenced_until"].is_string());
        assert!(status["server_time"].is_string());
        assert!(status.get("reason").is_none());
        assert_eq!(
            request(
                &app,
                Method::PUT,
                &path,
                &mod_cookie,
                Some(silence(expiry, 0))
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let (code, stale_lift) = request(
            &app,
            Method::DELETE,
            &path,
            &mod_cookie,
            Some(json!({"reason":"lift","expected_revision":0})),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(stale_lift["error"]["code"], "chat_silence_conflict");
        assert_eq!(audit_count(&state.db).await, before + 1);
        let (code, lifted) = request(
            &app,
            Method::DELETE,
            &path,
            &mod_cookie,
            Some(json!({"reason":"appeal accepted","expected_revision":1})),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{lifted}");
        assert_eq!(lifted["chat_silence_revision"], 2);
        assert!(lifted["chat_silenced_until"].is_null());
        let (_, status) = request(
            &app,
            Method::GET,
            "/api/v1/chat/status",
            &player_cookie,
            None,
        )
        .await;
        assert_eq!(status["can_send"], true);
        assert!(status["send_restriction"].is_null());
        assert_eq!(
            request(
                &app,
                Method::PUT,
                &format!("/api/v1/admin/users/{peer}/chat-silence"),
                &admin_cookie,
                Some(silence(expiry, 0))
            )
            .await
            .0,
            StatusCode::OK
        );
        let (_, status) =
            request(&app, Method::GET, "/api/v1/chat/status", &peer_cookie, None).await;
        assert_eq!(status["send_restriction"], "no_permission");
        assert_eq!(
            request(
                &app,
                Method::PUT,
                &format!("/api/v1/admin/users/{outsider}/chat-silence"),
                &mod_cookie,
                Some(silence(expiry, 0))
            )
            .await
            .0,
            StatusCode::OK
        );
        let audits = audit_count(&state.db).await;
        for invalid_expiry in [
            Utc::now() - Duration::seconds(1),
            Utc::now() + Duration::days(31),
        ] {
            assert_eq!(
                request(
                    &app,
                    Method::PUT,
                    &path,
                    &mod_cookie,
                    Some(silence(invalid_expiry, 2))
                )
                .await
                .0,
                StatusCode::BAD_REQUEST
            );
        }
        for invalid in [
            json!({"reason":"x"}),
            json!({"reason":"x","expected_revision":2,"extra":true}),
        ] {
            assert_eq!(
                request(&app, Method::DELETE, &path, &mod_cookie, Some(invalid))
                    .await
                    .0,
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
        assert_eq!(audit_count(&state.db).await, audits);
    })
    .await;
    fixture.finish(outcome).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn timed_chat_silence_prevents_new_public_and_pm_sends_preserving_retries_and_reads() {
    let fixture = MutationFixture::new("chat_silence_send", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let outcome = tokio::spawn(async move {
        let (_,_,_,mod_cookie) = actor(&state,"SendModerator",&["chat.moderate"]).await;
        let (sender,_,_,sender_cookie) = actor(&state,"MutedSender",&["chat.send"]).await;
        let (recipient,_,_,recipient_cookie) = actor(&state,"IncomingSender",&["chat.send"]).await;
        let public: i64 = sqlx::query_scalar("INSERT INTO channels(name,description) VALUES('#silence-proof','fixture') RETURNING id")
            .fetch_one(&state.db).await.unwrap();
        let (code,pm) = request(&app,Method::POST,"/api/v1/conversations",&sender_cookie,Some(json!({"target_id":recipient}))).await;
        assert_eq!(code,StatusCode::OK,"{pm}");
        let pm = pm["id"].as_i64().unwrap();
        let public_path = format!("/api/v1/channels/{public}/messages");
        let private_path = format!("/api/v1/conversations/{pm}/messages");
        let mut accepted = Vec::new();
        for (path,uuid) in [(&public_path,"silence-public-retry"),(&private_path,"silence-pm-retry")] {
            let input = json!({"content":"accepted before silence","client_uuid":uuid});
            let (code,saved) = request(&app,Method::POST,path,&sender_cookie,Some(input.clone())).await;
            assert_eq!(code,StatusCode::OK,"{saved}");
            accepted.push((path.clone(),input,saved["id"].as_i64().unwrap()));
        }
        let silence_path = format!("/api/v1/admin/users/{sender}/chat-silence");
        assert_eq!(request(&app,Method::PUT,&silence_path,&mod_cookie,Some(silence(Utc::now()+Duration::hours(1),0))).await.0,StatusCode::OK);
        let before = message_count(&state.db).await;
        for (path,input,id) in &accepted {
            let (code,reused) = request(&app,Method::POST,path,&sender_cookie,Some(input.clone())).await;
            assert_eq!(code,StatusCode::OK,"accepted identities remain reusable while muted: {reused}");
            assert_eq!(reused["id"],*id);
            let new = json!({"content":"must not be stored","client_uuid":format!("new-{id}")});
            let (code,rejected) = request(&app,Method::POST,path,&sender_cookie,Some(new)).await;
            assert_eq!(code,StatusCode::FORBIDDEN,"{rejected}");
            assert_eq!(rejected["error"]["code"],"chat_silenced");
            assert!(rejected["error"]["message"].as_str().unwrap().contains("until"));
            assert_eq!(request(&app,Method::GET,path,&sender_cookie,None).await.0,StatusCode::OK);
        }
        assert_eq!(message_count(&state.db).await,before,"neither bridge nor website table gets denied messages");
        let (_,detail) = request(&app,Method::GET,&format!("/api/v1/conversations/{pm}"),&sender_cookie,None).await;
        assert_eq!(detail["send_restriction"],"silenced");
        assert!(detail["silenced_until"].is_string());
        let (code,incoming) = request(&app,Method::POST,&private_path,&recipient_cookie,Some(json!({"content":"incoming remains available","client_uuid":"incoming-after-mute"}))).await;
        assert_eq!(code,StatusCode::OK,"{incoming}");
        assert_eq!(request(&app,Method::PUT,&format!("/api/v1/conversations/{pm}/read"),&sender_cookie,Some(json!({"message_id":incoming["id"]}))).await.0,StatusCode::NO_CONTENT);
        assert_eq!(request(&app,Method::POST,"/api/v1/conversations",&sender_cookie,Some(json!({"target_id":recipient}))).await.0,StatusCode::OK,"silence does not remove joins or reading membership");
        // Expiry requires no scheduler or delete: presentation normalizes it,
        // while the raw revision retains every actual policy change.
        sqlx::query("UPDATE users SET chat_silenced_until=clock_timestamp()-interval '1 second' WHERE id=$1")
            .bind(sender).execute(&state.db).await.unwrap();
        let raw = revision(&state.db,sender).await;
        assert_eq!(raw,2);
        let (_,status) = request(&app,Method::GET,"/api/v1/chat/status",&sender_cookie,None).await;
        assert_eq!(status["can_send"],true);
        assert!(status["silenced_until"].is_null());
        let (_,directory) = request(&app,Method::GET,&format!("/api/v1/admin/chat-silences?search={sender}"),&mod_cookie,None).await;
        assert!(directory["items"][0]["chat_silenced_until"].is_null());
        assert_eq!(directory["items"][0]["chat_silence_revision"],raw);
        assert_eq!(request(&app,Method::POST,&public_path,&sender_cookie,Some(json!({"content":"after expiry","client_uuid":"after-expiry"}))).await.0,StatusCode::OK);
    }).await;
    fixture.finish(outcome).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn queued_chat_moderation_rechecks_revocation_and_target_protection() {
    let fixture = MutationFixture::new("chat_auth", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let application = fixture.schema.clone();
    let outcome = tokio::spawn(async move {
        for (index,change) in ["permission","session","restriction","target"].into_iter().enumerate() {
            for (suffix,method) in [("apply",Method::PUT),("lift",Method::DELETE)] {
                let (moderator,role,sid,cookie) = actor(&state,&format!("QueueMod{index}{suffix}"),&["chat.moderate"]).await;
                let (target,target_role,_,_) = actor(&state,&format!("QueueTarget{index}{suffix}"),&["chat.send"]).await;
                let path = format!("/api/v1/admin/users/{target}/chat-silence");
                let input = if method==Method::PUT { silence(Utc::now()+Duration::hours(1),0) }
                    else { json!({"reason":"lift","expected_revision":0}) };
                let before = audit_count(&state.db).await;
                let mut blocking = state.db.begin().await.unwrap();
                sqlx::query("LOCK TABLE roles,user_roles IN SHARE ROW EXCLUSIVE MODE")
                    .execute(&mut *blocking).await.unwrap();
                let queued_app = app.clone();
                let queued = tokio::spawn(async move { request(&queued_app,method,&path,&cookie,Some(input)).await });
                wait_for_lock(&state.db,&application,"LOCK TABLE roles,user_roles IN SHARE MODE").await;
                match change {
                    "permission" => { sqlx::query("UPDATE roles SET permissions='{}' WHERE id=$1").bind(role).execute(&mut *blocking).await.unwrap(); }
                    "session" => { sqlx::query("DELETE FROM sessions WHERE id=$1").bind(sid).execute(&mut *blocking).await.unwrap(); }
                    "restriction" => { sqlx::query("UPDATE users SET is_restricted=true WHERE id=$1").bind(moderator).execute(&mut *blocking).await.unwrap(); }
                    "target" => { sqlx::query("UPDATE roles SET permissions=ARRAY['chat.send','chat.moderate'] WHERE id=$1").bind(target_role).execute(&mut *blocking).await.unwrap(); }
                    _ => unreachable!(),
                }
                blocking.commit().await.unwrap();
                let (code,value) = tokio::time::timeout(StdDuration::from_secs(5),queued).await.unwrap().unwrap();
                assert_eq!(code,if matches!(change,"session"|"restriction") {StatusCode::UNAUTHORIZED} else {StatusCode::FORBIDDEN},"{change} {suffix}: {value}");
                assert_eq!(revision(&state.db,target).await,0);
                assert_eq!(audit_count(&state.db).await,before);
            }
        }
    }).await;
    fixture.finish(outcome).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn chat_silence_deadlines_are_checked_after_real_lock_waits() {
    let fixture = MutationFixture::new("chat_silence_deadline", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let application = fixture.schema.clone();
    let outcome = tokio::spawn(async move {
        let (_,_,_,mod_cookie) = actor(&state,"DeadlineModerator",&["chat.moderate"]).await;
        let (target,_,_,target_cookie) = actor(&state,"DeadlinePlayer",&["chat.send"]).await;
        let path = format!("/api/v1/admin/users/{target}/chat-silence");
        let expiry: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()+interval '1500 milliseconds'").fetch_one(&state.db).await.unwrap();
        let before = audit_count(&state.db).await;
        let mut blocking = state.db.begin().await.unwrap();
        sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE").bind(target).fetch_one(&mut *blocking).await.unwrap();
        let queued_app = app.clone();
        let queued = tokio::spawn(async move { request(&queued_app,Method::PUT,&path,&mod_cookie,Some(silence(expiry,0))).await });
        wait_for_lock(&state.db,&application,"SELECT id FROM users WHERE id=ANY($1)").await;
        wait_until_expired(&state.db,expiry).await;
        blocking.commit().await.unwrap();
        let (code,value) = queued.await.unwrap();
        assert_eq!(code,StatusCode::BAD_REQUEST,"expiry that passes behind a target lock is not applied: {value}");
        assert_eq!(revision(&state.db,target).await,0);
        assert_eq!(audit_count(&state.db).await,before);

        let expiry: DateTime<Utc> = sqlx::query_scalar("UPDATE users SET chat_silenced_until=clock_timestamp()+interval '1500 milliseconds' WHERE id=$1 RETURNING chat_silenced_until")
            .bind(target).fetch_one(&state.db).await.unwrap();
        let channel: i64 = sqlx::query_scalar("INSERT INTO channels(name,description) VALUES('#deadline','fixture') RETURNING id").fetch_one(&state.db).await.unwrap();
        let mut blocking = state.db.begin().await.unwrap();
        sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE").bind(target).fetch_one(&mut *blocking).await.unwrap();
        let queued_app = app.clone();
        let queued = tokio::spawn(async move { request(&queued_app,Method::POST,&format!("/api/v1/channels/{channel}/messages"),&target_cookie,Some(json!({"content":"expiry passed while waiting","client_uuid":"queued-expiry-send"}))).await });
        wait_for_lock(&state.db,&application,"SELECT id FROM users WHERE id=ANY($1)").await;
        wait_until_expired(&state.db,expiry).await;
        blocking.commit().await.unwrap();
        let (code,value) = queued.await.unwrap();
        assert_eq!(code,StatusCode::OK,"expired silence cannot deny a post-wait message: {value}");
        assert_eq!(message_count(&state.db).await,(1,0));

        // An API moderation change holds the same sender row. A request queued
        // behind it must see the committed silence rather than its entry state.
        let (actor_id,_,_,cookie) = actor(&state,"ConcurrentMuteSender",&["chat.send"]).await;
        let mut blocking = state.db.begin().await.unwrap();
        sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE").bind(actor_id).fetch_one(&mut *blocking).await.unwrap();
        let queued_app = app.clone();
        let queued = tokio::spawn(async move { request(&queued_app,Method::POST,&format!("/api/v1/channels/{channel}/messages"),&cookie,Some(json!({"content":"silence committed ahead of send","client_uuid":"queued-concurrent-mute"}))).await });
        wait_for_lock(&state.db,&application,"SELECT id FROM users WHERE id=ANY($1)").await;
        sqlx::query("UPDATE users SET chat_silenced_until=clock_timestamp()+interval '1 hour' WHERE id=$1").bind(actor_id).execute(&mut *blocking).await.unwrap();
        blocking.commit().await.unwrap();
        assert_eq!(queued.await.unwrap().0,StatusCode::FORBIDDEN);
        assert_eq!(message_count(&state.db).await,(1,0));
    }).await;
    fixture.finish(outcome).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn concurrent_chat_apply_and_lift_cannot_overwrite_a_newer_policy() {
    let fixture = MutationFixture::new("chat_silence_cas", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let application = fixture.schema.clone();
    let outcome = tokio::spawn(async move {
        let (_, _, _, mod_cookie) = actor(&state, "FirstCasModerator", &["chat.moderate"]).await;
        let (_, _, _, other_cookie) = actor(&state, "SecondCasModerator", &["chat.moderate"]).await;
        let (target, _, _, _) = actor(&state, "CasPlayer", &["chat.send"]).await;
        let path = format!("/api/v1/admin/users/{target}/chat-silence");
        assert_eq!(
            request(
                &app,
                Method::PUT,
                &path,
                &mod_cookie,
                Some(silence(Utc::now() + Duration::hours(1), 0))
            )
            .await
            .0,
            StatusCode::OK
        );
        let before = audit_count(&state.db).await;
        let mut blocking = state.db.begin().await.unwrap();
        sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
            .bind(target)
            .fetch_one(&mut *blocking)
            .await
            .unwrap();
        let apply_app = app.clone();
        let apply_path = path.clone();
        let apply = tokio::spawn(async move {
            request(
                &apply_app,
                Method::PUT,
                &apply_path,
                &mod_cookie,
                Some(silence(Utc::now() + Duration::hours(2), 1)),
            )
            .await
        });
        let lift_app = app.clone();
        let lift = tokio::spawn(async move {
            request(
                &lift_app,
                Method::DELETE,
                &path,
                &other_cookie,
                Some(json!({"reason":"concurrent appeal","expected_revision":1})),
            )
            .await
        });
        wait_for_lock(
            &state.db,
            &application,
            "SELECT id FROM users WHERE id=ANY($1)",
        )
        .await;
        blocking.commit().await.unwrap();
        let results = [apply.await.unwrap(), lift.await.unwrap()];
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
        assert_eq!(revision(&state.db, target).await, 2);
        assert_eq!(audit_count(&state.db).await, before + 1);
        let winner = results
            .iter()
            .find(|(code, _)| *code == StatusCode::OK)
            .unwrap();
        let stored: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT chat_silenced_until FROM users WHERE id=$1")
                .bind(target)
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(winner.1["chat_silenced_until"].is_null(), stored.is_none());
    })
    .await;
    fixture.finish(outcome).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn chat_silence_audit_failure_rolls_back_policy_and_revision() {
    let fixture = MutationFixture::new("chat_silence_rollback", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let outcome = tokio::spawn(async move {
        let (_, _, _, cookie) = actor(&state, "RollbackChatModerator", &["chat.moderate"]).await;
        let (target, _, _, _) = actor(&state, "RollbackChatPlayer", &["chat.send"]).await;
        reject_audits_at_commit(&state.db).await;
        assert_eq!(
            request(
                &app,
                Method::PUT,
                &format!("/api/v1/admin/users/{target}/chat-silence"),
                &cookie,
                Some(silence(Utc::now() + Duration::hours(1), 0))
            )
            .await
            .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(revision(&state.db, target).await, 0);
        assert_eq!(audit_count(&state.db).await, 0);
        let until: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT chat_silenced_until FROM users WHERE id=$1")
                .bind(target)
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert!(until.is_none());
    })
    .await;
    fixture.finish(outcome).await;
}
