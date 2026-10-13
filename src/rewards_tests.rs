//! Rewards security and accounting checks use only an owned disposable PG17 schema.
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
use chrono::{DateTime, Datelike, Duration as ChronoDuration, FixedOffset, SecondsFormat, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

const ADMIN: &str = "/api/v1/admin/rewards";
const ME: &str = "/api/v1/me/rewards";
const PURCHASE: &str = "/api/v1/me/rewards/purchases";
const EQUIPMENT: &str = "/api/v1/me/rewards/equipment";

async fn request(
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

async fn get_state(app: &Router, cookie: &str) -> Value {
    let (status, state) = request(app, "GET", ME, Some(cookie), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{state}");
    state
}

fn reward_body(name: &str, kind: &str, price: i64) -> Value {
    json!({
        "name":name,
        "description":"Owned integration fixture",
        "kind":kind,
        "price":price,
        "color":"#88AAFF",
        "style":if kind == "avatar_frame" { Some("line") } else { None },
        "active":true
    })
}

async fn create(app: &Router, cookie: &str, name: &str, kind: &str, price: i64) -> Value {
    let (status, item) = request(
        app,
        "POST",
        ADMIN,
        Some(cookie),
        reward_body(name, kind, price),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{item}");
    assert_eq!(item["revision"], 1);
    assert!(item["id"].as_i64().unwrap() > 0);
    item
}

fn edit_body(item: &Value) -> Value {
    json!({
        "name":item["name"],
        "description":item["description"],
        "kind":item["kind"],
        "price":item["price"],
        "color":item["color"],
        "style":item["style"],
        "active":item["active"],
        "revision":item["revision"]
    })
}

async fn patch(app: &Router, cookie: &str, item: &Value, body: Value) -> Value {
    let (status, changed) = request(
        app,
        "PATCH",
        &format!("{ADMIN}/{}", item["id"]),
        Some(cookie),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(
        changed["revision"].as_i64(),
        item["revision"].as_i64().map(|revision| revision + 1)
    );
    changed
}

async fn purchase(app: &Router, cookie: &str, item: &Value) -> (StatusCode, Value) {
    request(
        app,
        "POST",
        PURCHASE,
        Some(cookie),
        json!({"reward_id":item["id"],"revision":item["revision"]}),
    )
    .await
}

async fn claim(app: &Router, cookie: &str, key: &str, week: &str) -> (StatusCode, Value) {
    request(
        app,
        "POST",
        &format!("{ME}/missions/{key}/claim"),
        Some(cookie),
        json!({"week_start":week}),
    )
    .await
}

fn mission<'a>(state: &'a Value, key: &str) -> &'a Value {
    state["missions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["key"] == key)
        .unwrap_or_else(|| panic!("missing mission {key}: {state}"))
}

fn iso(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

async fn current_week(db: &PgPool) -> DateTime<Utc> {
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(db)
        .await
        .unwrap();
    let local = now.with_timezone(&FixedOffset::east_opt(8 * 3600).unwrap());
    let monday = local.date_naive()
        - ChronoDuration::days(i64::from(local.weekday().num_days_from_monday()));
    monday
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(FixedOffset::east_opt(8 * 3600).unwrap())
        .unwrap()
        .with_timezone(&Utc)
}

async fn balance(db: &PgPool, user: i64, amount: i64) {
    sqlx::query(
        "INSERT INTO reward_wallets(user_id,balance) VALUES($1,$2) \
         ON CONFLICT(user_id) DO UPDATE SET balance=EXCLUDED.balance",
    )
    .bind(user)
    .bind(amount)
    .execute(db)
    .await
    .unwrap();
}

async fn owned(db: &PgPool, user: i64, item: &Value) {
    sqlx::query(
        "INSERT INTO reward_inventory(user_id,reward_id,acquired_at,paid) \
         VALUES($1,$2,clock_timestamp(),$3)",
    )
    .bind(user)
    .bind(item["id"].as_i64().unwrap())
    .bind(item["price"].as_i64().unwrap())
    .execute(db)
    .await
    .unwrap();
}

async fn seed_map(db: &PgPool, mode: i16) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO beatmaps(title,artist,creator,version,mode,status,stars,md5,osu_path) \
         VALUES('Rewards fixture','Illyaverse','Integration','Easy',$1,'ranked',1,$2,'fixture.osu') \
         RETURNING id",
    )
    .bind(mode)
    .bind(Uuid::new_v4().simple().to_string())
    .fetch_one(db)
    .await
    .unwrap()
}

#[allow(clippy::too_many_arguments)]
async fn seed_score(
    db: &PgPool,
    user: i64,
    map: i64,
    mode: i16,
    variant: &str,
    verification: &str,
    passed: bool,
    created: DateTime<Utc>,
    origin: &str,
    replay_played: Option<DateTime<Utc>>,
) {
    sqlx::query(
        "INSERT INTO scores(user_id,beatmap_id,mode,score,accuracy,max_combo,pp,score_variant,\
         verification,passed,created_at,submission_origin,replay_played_at,mods) \
         VALUES($1,$2,$3,1234,100,2,1,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(user)
    .bind(map)
    .bind(mode)
    .bind(variant)
    .bind(verification)
    .bind(passed)
    .bind(created)
    .bind(origin)
    .bind(replay_played)
    .bind(if variant == "relax" { 128_i64 } else { 0_i64 })
    .execute(db)
    .await
    .unwrap();
}

async fn complete_missions(db: &PgPool, user: i64, week: DateTime<Utc>) {
    let maps = [
        seed_map(db, 0).await,
        seed_map(db, 1).await,
        seed_map(db, 0).await,
    ];
    for (index, map) in [maps[0], maps[0], maps[1], maps[1], maps[2]]
        .into_iter()
        .enumerate()
    {
        seed_score(
            db,
            user,
            map,
            if index == 2 || index == 3 { 1 } else { 0 },
            "vanilla",
            "verified",
            true,
            week + ChronoDuration::hours(1),
            "client",
            None,
        )
        .await;
    }
}

async fn snapshot(db: &PgPool) -> Value {
    let mut value = serde_json::Map::new();
    for table in [
        "reward_catalog",
        "reward_wallets",
        "reward_mission_claims",
        "reward_inventory",
        "reward_equipment",
        "reward_ledger",
        "audit_events",
    ] {
        let rows: Value = sqlx::query_scalar(&format!(
            "SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY to_jsonb(r)::text),'[]'::jsonb) \
             FROM {table} r"
        ))
        .fetch_one(db)
        .await
        .unwrap();
        value.insert(table.into(), rows);
    }
    Value::Object(value)
}

async fn accounting(db: &PgPool, user: i64) -> (i64, i64, i64) {
    sqlx::query_as(
        "SELECT COALESCE((SELECT balance FROM reward_wallets WHERE user_id=$1),0),\
         (SELECT count(*) FROM reward_inventory WHERE user_id=$1),\
         (SELECT count(*) FROM reward_ledger WHERE user_id=$1)",
    )
    .bind(user)
    .fetch_one(db)
    .await
    .unwrap()
}

fn assert_card_cosmetics(card: &Value, cosmetics: &Value) {
    assert_eq!(card["cosmetics"], *cosmetics, "{card}");
    for private_key in [
        "balance",
        "inventory",
        "missions",
        "ledger",
        "equipment",
        "permissions",
        "email",
        "password_hash",
    ] {
        assert!(
            card.get(private_key).is_none(),
            "public card leaked {private_key}: {card}"
        );
    }
    let object = card["cosmetics"].as_object().unwrap();
    assert_eq!(object.len(), 2);
    for key in ["title", "avatar_frame"] {
        if let Some(reward) = object[key].as_object() {
            assert_eq!(reward.len(), 10, "public reward fields: {reward:?}");
            for field in [
                "id",
                "name",
                "description",
                "kind",
                "price",
                "color",
                "style",
                "active",
                "revision",
            ] {
                assert!(reward.contains_key(field), "missing public field {field}");
            }
        }
    }
}

fn listed_card(items: &Value, user: i64) -> &Value {
    items
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == user)
        .unwrap_or_else(|| panic!("missing card {user}: {items}"))
}

fn relationship_card(items: &Value, user: i64) -> &Value {
    &items
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["target_id"] == user)
        .unwrap_or_else(|| panic!("missing relationship {user}: {items}"))["target"]
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn rewards_public_cards_show_only_equipped_cosmetics_and_preserve_visibility() {
    let fixture = MutationFixture::new("rewards_public_cards", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (_, _, _, manager) = actor(&state, "CardRewardManager", &["rewards.manage"]).await;
        let (owner, badge, _, owner_cookie) = actor(&state, "CardRewardOwner", &[]).await;
        let (_, _, _, viewer_cookie) = actor(&state, "CardRewardViewer", &["social.manage"]).await;
        let (bare, _, _, _) = actor(&state, "CardRewardBare", &[]).await;
        let (restricted, _, _, _) = actor(&state, "CardRewardRestricted", &[]).await;
        sqlx::query(
            "UPDATE roles SET display_badge=true,short_name='CARD',colour='#88aaff',rank=20 \
             WHERE id=$1",
        )
        .bind(badge)
        .execute(&state.db)
        .await
        .unwrap();
        for user in [bare, restricted] {
            sqlx::query("INSERT INTO user_roles(user_id,role_id) VALUES($1,$2)")
                .bind(user)
                .bind(badge)
                .execute(&state.db)
                .await
                .unwrap();
        }
        for user in [owner, restricted] {
            sqlx::query(
                "INSERT INTO bancho_sessions(id,user_id,token_hash,expires_at) \
                 VALUES($1,$2,$3,now()+interval '1 hour')",
            )
            .bind(Uuid::new_v4())
            .bind(user)
            .bind(Uuid::new_v4().simple().to_string())
            .execute(&state.db)
            .await
            .unwrap();
        }
        for user in [owner, bare, restricted] {
            let added = request(
                &app,
                "POST",
                &format!("/api/v1/me/relationships/{user}"),
                Some(&viewer_cookie),
                json!({"relation_type":"friend"}),
            )
            .await;
            assert_eq!(added.0, StatusCode::OK, "{}", added.1);
        }
        sqlx::query("UPDATE users SET is_restricted=true WHERE id=$1")
            .bind(restricted)
            .execute(&state.db)
            .await
            .unwrap();
        balance(&state.db, owner, 12345).await;
        let title = create(&app, &manager, "Card equipped title", "title", 10).await;
        let frame = create(&app, &manager, "Card equipped frame", "avatar_frame", 20).await;
        let unequipped = create(&app, &manager, "Card private inventory", "title", 30).await;
        for item in [&title, &frame, &unequipped] {
            owned(&state.db, owner, item).await;
        }
        let equipped = request(
            &app,
            "PATCH",
            EQUIPMENT,
            Some(&owner_cookie),
            json!({"title_id":title["id"],"avatar_frame_id":frame["id"]}),
        )
        .await;
        assert_eq!(equipped.0, StatusCode::OK, "{}", equipped.1);
        for item in [&title, &frame] {
            let mut body = edit_body(item);
            body["active"] = json!(false);
            patch(&app, &manager, item, body).await;
        }
        let empty = json!({"title":null,"avatar_frame":null});
        let mut relation = "friends";
        for step in 0..3 {
            if step == 1 {
                // Blocking removes the friendship, while the existing blocks
                // view remains allowed to display that target's public card.
                let blocked = request(
                    &app,
                    "POST",
                    &format!("/api/v1/me/relationships/{owner}"),
                    Some(&viewer_cookie),
                    json!({"relation_type":"block"}),
                )
                .await;
                assert_eq!(blocked.0, StatusCode::OK, "{}", blocked.1);
                relation = "blocks";
            } else if step == 2 {
                let unset = request(
                    &app,
                    "PATCH",
                    EQUIPMENT,
                    Some(&owner_cookie),
                    json!({"title_id":null,"avatar_frame_id":null}),
                )
                .await;
                assert_eq!(unset.0, StatusCode::OK, "{}", unset.1);
            }
            let before = snapshot(&state.db).await;
            let profile = request(
                &app,
                "GET",
                &format!("/api/v1/users/{owner}"),
                None,
                Value::Null,
            )
            .await;
            assert_eq!(profile.0, StatusCode::OK, "{}", profile.1);
            let cosmetics = &profile.1["cosmetics"];
            if step < 2 {
                assert_eq!(cosmetics["title"]["id"], title["id"]);
                assert_eq!(cosmetics["avatar_frame"]["id"], frame["id"]);
                assert_eq!(cosmetics["title"]["active"], false);
                assert_eq!(cosmetics["avatar_frame"]["active"], false);
            } else {
                assert_eq!(*cosmetics, empty);
            }
            let online = request(&app, "GET", "/api/v1/users/online", None, Value::Null).await;
            assert_eq!(online.0, StatusCode::OK, "{}", online.1);
            let card = listed_card(&online.1["items"], owner);
            assert_card_cosmetics(card, cosmetics);
            assert_eq!(card["online"], true);
            assert_eq!(listed_card(&card["groups"], badge)["short_name"], "CARD");
            assert!(
                online.1["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| { item["id"] != restricted && item["id"] != bare })
            );
            let groups = request(&app, "GET", "/api/v1/groups", None, Value::Null).await;
            assert_eq!(groups.0, StatusCode::OK, "{}", groups.1);
            let members = &listed_card(&groups.1["items"], badge)["members"];
            assert_card_cosmetics(listed_card(members, owner), cosmetics);
            assert_card_cosmetics(listed_card(members, bare), &empty);
            assert!(
                members
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| item["id"] != restricted)
            );
            let relationships = request(
                &app,
                "GET",
                "/api/v1/me/relationships",
                Some(&viewer_cookie),
                Value::Null,
            )
            .await;
            assert_eq!(relationships.0, StatusCode::OK, "{}", relationships.1);
            assert_card_cosmetics(
                relationship_card(&relationships.1[relation], owner),
                cosmetics,
            );
            let bare_card = relationship_card(&relationships.1["friends"], bare);
            assert_card_cosmetics(bare_card, &empty);
            assert_eq!(bare_card["online"], false);
            for key in ["friends", "blocks"] {
                assert!(relationships.1[key].as_array().unwrap().iter().all(|item| {
                    item["target_id"] != restricted
                        && (key == relation || item["target_id"] != owner)
                }));
            }
            assert_eq!(
                snapshot(&state.db).await,
                before,
                "card reads must not mutate rewards"
            );
        }
        sqlx::query("UPDATE users SET is_restricted=true WHERE id=$1")
            .bind(owner)
            .execute(&state.db)
            .await
            .unwrap();
        for path in [
            "/api/v1/users/online",
            "/api/v1/groups",
            "/api/v1/me/relationships",
        ] {
            let (status, value) =
                request(&app, "GET", path, Some(&viewer_cookie), Value::Null).await;
            assert_eq!(status, StatusCode::OK, "{value}");
            let cards = if path == "/api/v1/groups" {
                listed_card(&value["items"], badge)["members"]
                    .as_array()
                    .unwrap()
            } else if path == "/api/v1/me/relationships" {
                value["blocks"].as_array().unwrap()
            } else {
                value["items"].as_array().unwrap()
            };
            assert!(
                cards
                    .iter()
                    .all(|card| card["id"] != owner && card["target_id"] != owner)
            );
        }
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn rewards_http_permissions_and_input_boundaries() {
    let fixture = MutationFixture::new("rewards_permissions", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (_, _, _, manager) = actor(&state, "RewardsManager", &["rewards.manage"]).await;
        let (_, _, _, player) = actor(&state, "RewardsPlayer", &[]).await;
        let (_, _, _, other_admin) = actor(
            &state,
            "RewardsUnrelatedAdmin",
            &["admin.view", "roles.manage"],
        )
        .await;
        let before = snapshot(&state.db).await;
        for cookie in [&player, &other_admin] {
            for (method, path, body) in [
                ("GET", ADMIN.into(), Value::Null),
                ("POST", ADMIN.into(), reward_body("Denied", "title", 10)),
                (
                    "PATCH",
                    format!("{ADMIN}/999999"),
                    json!({
                        "name":"Denied","description":"","kind":"title","price":10,
                        "color":null,"style":null,"active":true,"revision":1
                    }),
                ),
                ("DELETE", format!("{ADMIN}/999999"), json!({"revision":1})),
            ] {
                let denied = request(&app, method, &path, Some(cookie), body.clone()).await;
                assert_eq!(denied.0, StatusCode::FORBIDDEN, "{method}: {}", denied.1);
                assert_eq!(
                    request(&app, method, &path, None, body).await.0,
                    StatusCode::UNAUTHORIZED
                );
            }
        }
        for (method, path, body) in [
            ("GET", ME, Value::Null),
            ("POST", PURCHASE, json!({"reward_id":1,"revision":1})),
            (
                "PATCH",
                EQUIPMENT,
                json!({"title_id":null,"avatar_frame_id":null}),
            ),
        ] {
            assert_eq!(
                request(&app, method, path, None, body).await.0,
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(snapshot(&state.db).await, before);
        let item = create(&app, &manager, "Permission title", "title", 10).await;
        let stable = snapshot(&state.db).await;
        for body in [
            json!({"title_id":null}),
            json!({"avatar_frame_id":null}),
            json!({"title_id":null,"avatar_frame_id":null,"user_id":999999}),
            json!({"title_id":-1,"avatar_frame_id":null}),
        ] {
            let denied = request(&app, "PATCH", EQUIPMENT, Some(&player), body).await;
            assert!(
                matches!(
                    denied.0,
                    StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
                ),
                "invalid equipment body: {} {}",
                denied.0,
                denied.1
            );
            assert_eq!(snapshot(&state.db).await, stable);
        }
        for body in [
            json!({"reward_id":item["id"],"revision":item["revision"],"price":0}),
            json!({"reward_id":item["id"],"revision":item["revision"],"user_id":999999}),
        ] {
            let denied = request(&app, "POST", PURCHASE, Some(&player), body).await;
            assert!(
                matches!(
                    denied.0,
                    StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
                ),
                "unknown purchase field: {} {}",
                denied.0,
                denied.1
            );
            assert_eq!(snapshot(&state.db).await, stable);
        }
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn rewards_weekly_eligibility_claims_and_parallel_idempotency() {
    let fixture = MutationFixture::new("rewards_missions", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (user, _, _, cookie) = actor(&state, "RewardsWeekly", &[]).await;
        let (other, _, _, _) = actor(&state, "RewardsWeeklyOther", &[]).await;
        let week = current_week(&state.db).await;
        let week_text = iso(week);
        let now = week + ChronoDuration::hours(1);
        let old = week - ChronoDuration::seconds(1);
        let future = week + ChronoDuration::days(7);
        let first = seed_map(&state.db, 0).await;
        let second = seed_map(&state.db, 0).await;
        let third = seed_map(&state.db, 1).await;
        for _ in 0..4 {
            seed_score(
                &state.db, user, first, 0, "vanilla", "verified", true, now, "client", None,
            )
            .await;
        }
        for (verification, passed, created, origin, replay) in [
            ("pending", true, now, "client", None),
            ("rejected", true, now, "client", None),
            ("verified", false, now, "client", None),
            ("verified", true, old, "client", None),
            ("verified", true, future, "client", None),
            ("verified", true, now, "replay", Some(old)),
            ("verified", true, old, "replay", Some(now)),
            ("verified", true, now, "replay", None),
            ("verified", true, now, "replay", Some(future)),
        ] {
            seed_score(
                &state.db,
                user,
                third,
                1,
                "vanilla",
                verification,
                passed,
                created,
                origin,
                replay,
            )
            .await;
        }
        complete_missions(&state.db, other, week).await;
        let initial = get_state(&app, &cookie).await;
        assert_eq!(initial["week_start"], week_text);
        assert_eq!(initial["resets_at"], iso(week + ChronoDuration::days(7)));
        assert_eq!(initial["balance"], 0);
        for (key, goal, progress) in [("play_5", 5, 4), ("maps_3", 3, 1), ("variety_2", 2, 1)] {
            let entry = mission(&initial, key);
            assert_eq!(entry["goal"], goal);
            assert_eq!(entry["progress"], progress);
            assert_eq!(entry["reward"], 20);
            assert_eq!(entry["claimed"], false);
        }
        let incomplete = claim(&app, &cookie, "play_5", &week_text).await;
        assert_eq!(incomplete.0, StatusCode::CONFLICT, "{}", incomplete.1);
        assert_eq!(incomplete.1["error"]["code"], "mission_incomplete");
        for stale in [week - ChronoDuration::days(7), future] {
            let denied = claim(&app, &cookie, "play_5", &iso(stale)).await;
            assert_eq!(denied.0, StatusCode::CONFLICT, "{}", denied.1);
            assert_eq!(denied.1["error"]["code"], "mission_week_changed");
        }
        assert_eq!(accounting(&state.db, user).await, (0, 0, 0));
        // A different score variant is a distinct variety even with the same ruleset.
        seed_score(
            &state.db, user, second, 0, "relax", "verified", true, now, "client", None,
        )
        .await;
        let fifth = get_state(&app, &cookie).await;
        assert_eq!(mission(&fifth, "play_5")["progress"], 5);
        assert_eq!(mission(&fifth, "maps_3")["progress"], 2);
        assert_eq!(mission(&fifth, "variety_2")["progress"], 2);
        seed_score(
            &state.db,
            user,
            third,
            1,
            "vanilla",
            "verified",
            true,
            now,
            "replay",
            Some(now),
        )
        .await;
        let complete = get_state(&app, &cookie).await;
        assert_eq!(mission(&complete, "play_5")["progress"], 5);
        assert_eq!(mission(&complete, "maps_3")["progress"], 3);
        assert_eq!(mission(&complete, "variety_2")["progress"], 2);
        let (left, right) = tokio::join!(
            claim(&app, &cookie, "play_5", &week_text),
            claim(&app, &cookie, "play_5", &week_text)
        );
        assert_eq!(left.0, StatusCode::OK, "{}", left.1);
        assert_eq!(right.0, StatusCode::OK, "{}", right.1);
        assert_ne!(left.1["idempotent"], right.1["idempotent"]);
        assert_eq!(accounting(&state.db, user).await, (20, 0, 1));
        for key in ["maps_3", "variety_2"] {
            let claimed = claim(&app, &cookie, key, &week_text).await;
            assert_eq!(claimed.0, StatusCode::OK, "{}", claimed.1);
            assert_eq!(claimed.1["idempotent"], false);
        }
        assert_eq!(accounting(&state.db, user).await, (60, 0, 3));
        let claims: i64 =
            sqlx::query_scalar("SELECT count(*) FROM reward_mission_claims WHERE user_id=$1")
                .bind(user)
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(claims, 3);
        sqlx::query("UPDATE scores SET verification='rejected' WHERE user_id=$1")
            .bind(user)
            .execute(&state.db)
            .await
            .unwrap();
        let stable = snapshot(&state.db).await;
        for key in ["play_5", "maps_3", "variety_2"] {
            let retry = claim(&app, &cookie, key, &week_text).await;
            assert_eq!(retry.0, StatusCode::OK, "{}", retry.1);
            assert_eq!(retry.1["idempotent"], true);
            assert_eq!(retry.1["state"]["balance"], 60);
        }
        assert_eq!(snapshot(&state.db).await, stable);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn rewards_parallel_purchases_never_overdraw_or_charge_retries() {
    let fixture = MutationFixture::new("rewards_purchases", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (_, _, _, manager) = actor(&state, "RewardsPurchaseManager", &["rewards.manage"]).await;
        let (user, _, _, cookie) = actor(&state, "RewardsTwoItems", &[]).await;
        let (same_user, _, _, same_cookie) = actor(&state, "RewardsSameItem", &[]).await;
        balance(&state.db, user, 100).await;
        balance(&state.db, same_user, 100).await;
        let first = create(&app, &manager, "Concurrent first", "title", 80).await;
        let second = create(&app, &manager, "Concurrent second", "avatar_frame", 80).await;
        let (left, right) = tokio::join!(
            purchase(&app, &cookie, &first),
            purchase(&app, &cookie, &second)
        );
        assert_eq!(
            usize::from(left.0.is_success()) + usize::from(right.0.is_success()),
            1,
            "{left:?} {right:?}"
        );
        let failure = if left.0.is_success() { &right } else { &left };
        assert_eq!(failure.0, StatusCode::CONFLICT);
        assert_eq!(failure.1["error"]["code"], "insufficient_stardust");
        assert_eq!(accounting(&state.db, user).await, (20, 1, 1));
        let winner = if left.0.is_success() { &first } else { &second };
        let mut changed_body = edit_body(winner);
        changed_body["price"] = json!(99);
        changed_body["active"] = json!(false);
        patch(&app, &manager, winner, changed_body).await;
        let stable = snapshot(&state.db).await;
        let retry = purchase(&app, &cookie, winner).await;
        assert_eq!(retry.0, StatusCode::OK, "{}", retry.1);
        assert_eq!(retry.1["idempotent"], true);
        assert_eq!(snapshot(&state.db).await, stable);
        let third = create(&app, &manager, "Concurrent same", "title", 80).await;
        let (left, right) = tokio::join!(
            purchase(&app, &same_cookie, &third),
            purchase(&app, &same_cookie, &third)
        );
        assert_eq!(left.0, StatusCode::OK, "{}", left.1);
        assert_eq!(right.0, StatusCode::OK, "{}", right.1);
        assert_ne!(left.1["idempotent"], right.1["idempotent"]);
        assert_eq!(accounting(&state.db, same_user).await, (20, 1, 1));
        let paid: i64 = sqlx::query_scalar(
            "SELECT paid FROM reward_inventory WHERE user_id=$1 AND reward_id=$2",
        )
        .bind(same_user)
        .bind(third["id"].as_i64().unwrap())
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(paid, 80);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn rewards_catalog_cas_delisting_equipment_and_private_ownership() {
    let fixture = MutationFixture::new("rewards_lifecycle", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (_, _, _, manager) =
            actor(&state, "RewardsLifecycleManager", &["rewards.manage"]).await;
        let (user, _, _, cookie) = actor(&state, "RewardsOwner", &[]).await;
        let (stranger, _, _, other) = actor(&state, "RewardsStranger", &[]).await;
        balance(&state.db, user, 100).await;
        balance(&state.db, stranger, 100).await;
        let title = create(&app, &manager, "Owned title", "title", 10).await;
        let frame = create(&app, &manager, "Owned frame", "avatar_frame", 15).await;
        let unowned = create(&app, &manager, "Unowned title", "title", 20).await;
        let mut revised_body = edit_body(&unowned);
        revised_body["price"] = json!(21);
        let mut competing_body = edit_body(&unowned);
        competing_body["price"] = json!(22);
        let path = format!("{ADMIN}/{}", unowned["id"]);
        let (left, right) = tokio::join!(
            request(&app, "PATCH", &path, Some(&manager), revised_body),
            request(&app, "PATCH", &path, Some(&manager), competing_body)
        );
        assert_eq!(
            usize::from(left.0.is_success()) + usize::from(right.0.is_success()),
            1,
            "{left:?} {right:?}"
        );
        let revised = if left.0.is_success() {
            assert_eq!(right.0, StatusCode::CONFLICT);
            assert_eq!(right.1["error"]["code"], "reward_conflict");
            left.1
        } else {
            assert_eq!(left.0, StatusCode::CONFLICT);
            assert_eq!(left.1["error"]["code"], "reward_conflict");
            right.1
        };
        assert_eq!(revised["revision"], 2);
        let stable = snapshot(&state.db).await;
        let stale = request(
            &app,
            "PATCH",
            &format!("{ADMIN}/{}", unowned["id"]),
            Some(&manager),
            edit_body(&unowned),
        )
        .await;
        assert_eq!(stale.0, StatusCode::CONFLICT, "{}", stale.1);
        assert_eq!(stale.1["error"]["code"], "reward_conflict");
        let stale_purchase = purchase(&app, &cookie, &unowned).await;
        assert_eq!(
            stale_purchase.0,
            StatusCode::CONFLICT,
            "{}",
            stale_purchase.1
        );
        assert_eq!(stale_purchase.1["error"]["code"], "reward_conflict");
        assert_eq!(snapshot(&state.db).await, stable);
        for item in [&title, &frame] {
            let bought = purchase(&app, &cookie, item).await;
            assert_eq!(bought.0, StatusCode::OK, "{}", bought.1);
        }
        let stable = snapshot(&state.db).await;
        for (body, expected, code) in [
            (
                json!({"title_id":revised["id"],"avatar_frame_id":null}),
                StatusCode::CONFLICT,
                "reward_not_owned",
            ),
            (
                json!({"title_id":frame["id"],"avatar_frame_id":null}),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                json!({"title_id":null,"avatar_frame_id":title["id"]}),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
        ] {
            let denied = request(&app, "PATCH", EQUIPMENT, Some(&cookie), body).await;
            assert_eq!(denied.0, expected, "{}", denied.1);
            assert_eq!(denied.1["error"]["code"], code);
            assert_eq!(snapshot(&state.db).await, stable);
        }
        let denied = request(
            &app,
            "PATCH",
            EQUIPMENT,
            Some(&other),
            json!({"title_id":title["id"],"avatar_frame_id":frame["id"]}),
        )
        .await;
        assert_eq!(denied.0, StatusCode::CONFLICT, "{}", denied.1);
        assert_eq!(denied.1["error"]["code"], "reward_not_owned");
        assert_eq!(snapshot(&state.db).await, stable);
        let private = get_state(&app, &other).await;
        assert_eq!(private["balance"], 100);
        assert_eq!(private["inventory"], json!([]));
        assert_eq!(
            private["equipment"],
            json!({"title":null,"avatar_frame":null})
        );
        let scoped = request(
            &app,
            "GET",
            &format!("{ME}?user_id={user}"),
            Some(&other),
            Value::Null,
        )
        .await;
        assert_eq!(scoped.0, StatusCode::OK, "{}", scoped.1);
        assert_eq!(
            scoped.1, private,
            "a query parameter cannot select another wallet"
        );
        let equip = request(
            &app,
            "PATCH",
            EQUIPMENT,
            Some(&cookie),
            json!({"title_id":title["id"],"avatar_frame_id":frame["id"]}),
        )
        .await;
        assert_eq!(equip.0, StatusCode::OK, "{}", equip.1);
        assert_eq!(equip.1["state"]["equipment"]["title"]["id"], title["id"]);
        assert_eq!(
            equip.1["state"]["equipment"]["avatar_frame"]["id"],
            frame["id"]
        );
        let mut body = edit_body(&title);
        body["active"] = json!(false);
        let hidden = patch(&app, &manager, &title, body).await;
        let denied = purchase(&app, &other, &hidden).await;
        assert_eq!(denied.0, StatusCode::CONFLICT, "{}", denied.1);
        assert_eq!(denied.1["error"]["code"], "reward_unavailable");
        assert_eq!(
            request(
                &app,
                "PATCH",
                EQUIPMENT,
                Some(&cookie),
                json!({"title_id":null,"avatar_frame_id":null})
            )
            .await
            .0,
            StatusCode::OK
        );
        let equipped = request(
            &app,
            "PATCH",
            EQUIPMENT,
            Some(&cookie),
            json!({"title_id":hidden["id"],"avatar_frame_id":frame["id"]}),
        )
        .await;
        assert_eq!(equipped.0, StatusCode::OK, "{}", equipped.1);
        let public = request(
            &app,
            "GET",
            &format!("/api/v1/users/{user}"),
            None,
            Value::Null,
        )
        .await;
        assert_eq!(public.0, StatusCode::OK, "{}", public.1);
        assert_eq!(public.1["cosmetics"], equipped.1["state"]["equipment"]);
        for private_key in [
            "balance",
            "inventory",
            "missions",
            "ledger",
            "permissions",
            "email",
        ] {
            assert!(
                public.1.get(private_key).is_none(),
                "public profile leaked {private_key}"
            );
        }
        let stable = snapshot(&state.db).await;
        let deletion = request(
            &app,
            "DELETE",
            &format!("{ADMIN}/{}", hidden["id"]),
            Some(&manager),
            json!({"revision":hidden["revision"]}),
        )
        .await;
        assert_eq!(deletion.0, StatusCode::CONFLICT, "{}", deletion.1);
        assert_eq!(deletion.1["error"]["code"], "reward_in_use");
        assert_eq!(snapshot(&state.db).await, stable);
        let deleted = request(
            &app,
            "DELETE",
            &format!("{ADMIN}/{}", revised["id"]),
            Some(&manager),
            json!({"revision":revised["revision"]}),
        )
        .await;
        assert_eq!(deleted.0, StatusCode::OK, "{}", deleted.1);
        assert_eq!(deleted.1, json!({"ok":true}));
        let remaining: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM reward_catalog WHERE id=$1)")
                .bind(revised["id"].as_i64().unwrap())
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert!(!remaining);
        assert_eq!(accounting(&state.db, user).await, (75, 2, 2));
        assert_eq!(accounting(&state.db, stranger).await, (100, 0, 0));
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn rewards_admin_mutations_recheck_queued_permissions_and_sessions() {
    let fixture = MutationFixture::new("rewards_admin_auth", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let application = fixture.schema.clone();
    let result = tokio::spawn(async move {
        let (_, _, _, manager) = actor(&state, "RewardsQueueSeeder", &["rewards.manage"]).await;
        for operation in ["create", "update", "delete"] {
            for change in ["permission", "role", "revoke", "restrict", "expire"] {
                let (user, role, sid, cookie) = actor(&state, &format!("RewardsAdmin{operation}{change}"), &["rewards.manage"]).await;
                let item = create(&app, &manager, &format!("Queue {operation} {change}"), "title", 10).await;
                let before = snapshot(&state.db).await;
                let mut blocker = state.db.begin().await.unwrap();
                sqlx::query("LOCK TABLE roles,user_roles IN SHARE ROW EXCLUSIVE MODE")
                    .execute(&mut *blocker).await.unwrap();
                let queued_app = app.clone();
                let (method, path, body) = match operation {
                    "create" => ("POST", ADMIN.to_owned(), reward_body("Must not publish", "title", 10)),
                    "update" => ("PATCH", format!("{ADMIN}/{}", item["id"]), edit_body(&item)),
                    _ => ("DELETE", format!("{ADMIN}/{}", item["id"]), json!({"revision":item["revision"]})),
                };
                let pending = tokio::spawn(async move { request(&queued_app, method, &path, Some(&cookie), body).await });
                wait_for_lock(&state.db, &application, "LOCK TABLE roles,user_roles IN SHARE MODE").await;
                match change {
                    "permission" => { sqlx::query("UPDATE roles SET permissions='{}' WHERE id=$1").bind(role).execute(&mut *blocker).await.unwrap(); }
                    "role" => { sqlx::query("DELETE FROM user_roles WHERE user_id=$1 AND role_id=$2").bind(user).bind(role).execute(&mut *blocker).await.unwrap(); }
                    "revoke" => { sqlx::query("DELETE FROM sessions WHERE id=$1").bind(sid).execute(&mut *blocker).await.unwrap(); }
                    "restrict" => { sqlx::query("UPDATE users SET is_restricted=true WHERE id=$1").bind(user).execute(&mut *blocker).await.unwrap(); }
                    _ => { sqlx::query("UPDATE sessions SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(sid).execute(&mut *blocker).await.unwrap(); }
                }
                blocker.commit().await.unwrap();
                let response = tokio::time::timeout(Duration::from_secs(5), pending).await.unwrap().unwrap();
                assert_eq!(response.0, if change == "permission" || change == "role" { StatusCode::FORBIDDEN } else { StatusCode::UNAUTHORIZED }, "{operation}/{change}: {}", response.1);
                assert_eq!(snapshot(&state.db).await, before);
            }
        }
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn rewards_player_mutations_recheck_queued_sessions_and_resource_expiry() {
    let fixture = MutationFixture::new("rewards_player_auth", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let application = fixture.schema.clone();
    let result = tokio::spawn(async move {
        let (_, _, _, manager) = actor(&state, "RewardsPlayerQueueSeeder", &["rewards.manage"]).await;
        let title = create(&app, &manager, "Queued player title", "title", 10).await;
        let week = current_week(&state.db).await;
        for operation in ["claim", "purchase", "equip"] {
            for change in ["revoke", "restrict", "expire"] {
                let (user, _, sid, cookie) = actor(&state, &format!("RewardsPlayer{operation}{change}"), &[]).await;
                balance(&state.db, user, 100).await;
                complete_missions(&state.db, user, week).await;
                if operation == "equip" { owned(&state.db, user, &title).await; }
                let before = snapshot(&state.db).await;
                let mut blocker = state.db.begin().await.unwrap();
                sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
                    .bind(user).fetch_one(&mut *blocker).await.unwrap();
                let queued_app = app.clone();
                let (method, path, body) = match operation {
                    "claim" => ("POST", format!("{ME}/missions/play_5/claim"), json!({"week_start":iso(week)})),
                    "purchase" => ("POST", PURCHASE.to_owned(), json!({"reward_id":title["id"],"revision":title["revision"]})),
                    _ => ("PATCH", EQUIPMENT.to_owned(), json!({"title_id":title["id"],"avatar_frame_id":null})),
                };
                let pending = tokio::spawn(async move { request(&queued_app, method, &path, Some(&cookie), body).await });
                wait_for_lock(&state.db, &application, "SELECT id FROM users WHERE id=ANY").await;
                match change {
                    "revoke" => { sqlx::query("DELETE FROM sessions WHERE id=$1").bind(sid).execute(&mut *blocker).await.unwrap(); }
                    "restrict" => { sqlx::query("UPDATE users SET is_restricted=true WHERE id=$1").bind(user).execute(&mut *blocker).await.unwrap(); }
                    _ => { sqlx::query("UPDATE sessions SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(sid).execute(&mut *blocker).await.unwrap(); }
                }
                blocker.commit().await.unwrap();
                let response = tokio::time::timeout(Duration::from_secs(5), pending).await.unwrap().unwrap();
                assert_eq!(response.0, StatusCode::UNAUTHORIZED, "{operation}/{change}: {}", response.1);
                assert_eq!(snapshot(&state.db).await, before);
            }
        }
        // This request acquires a valid actor before a wallet wait. The wall
        // clock expires the session during that wait without a revocation race.
        let (user, _, sid, cookie) = actor(&state, "RewardsWalletDeadline", &[]).await;
        balance(&state.db, user, 100).await;
        let expiry: DateTime<Utc> = sqlx::query_scalar(
            "UPDATE sessions SET expires_at=clock_timestamp()+interval '1 second' WHERE id=$1 RETURNING expires_at",
        ).bind(sid).fetch_one(&state.db).await.unwrap();
        let before = snapshot(&state.db).await;
        let mut blocker = state.db.begin().await.unwrap();
        sqlx::query("SELECT user_id FROM reward_wallets WHERE user_id=$1 FOR UPDATE")
            .bind(user).fetch_one(&mut *blocker).await.unwrap();
        let queued_app = app.clone();
        let pending = tokio::spawn(async move { purchase(&queued_app, &cookie, &title).await });
        wait_for_lock(&state.db, &application, "reward_wallets").await;
        wait_until_expired(&state.db, expiry).await;
        blocker.commit().await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), pending).await.unwrap().unwrap();
        assert_eq!(response.0, StatusCode::UNAUTHORIZED, "{}", response.1);
        assert_eq!(snapshot(&state.db).await, before);
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn rewards_audit_commit_failure_rolls_back_all_mutations() {
    let fixture = MutationFixture::new("rewards_rollback", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (_, _, _, manager) = actor(&state, "RewardsRollbackManager", &["rewards.manage"]).await;
        let (user, _, _, cookie) = actor(&state, "RewardsRollbackPlayer", &[]).await;
        balance(&state.db, user, 100).await;
        let week = current_week(&state.db).await;
        complete_missions(&state.db, user, week).await;
        let title = create(&app, &manager, "Rollback owned", "title", 10).await;
        let unowned = create(&app, &manager, "Rollback unowned", "avatar_frame", 20).await;
        owned(&state.db, user, &title).await;
        let before = snapshot(&state.db).await;
        reject_audits_at_commit(&state.db).await;
        for (method, path, cookie, body) in [
            (
                "POST",
                ADMIN.to_owned(),
                manager.as_str(),
                reward_body("Rollback new", "title", 10),
            ),
            (
                "PATCH",
                format!("{ADMIN}/{}", unowned["id"]),
                manager.as_str(),
                edit_body(&unowned),
            ),
            (
                "DELETE",
                format!("{ADMIN}/{}", unowned["id"]),
                manager.as_str(),
                json!({"revision":unowned["revision"]}),
            ),
            (
                "POST",
                format!("{ME}/missions/play_5/claim"),
                cookie.as_str(),
                json!({"week_start":iso(week)}),
            ),
            (
                "POST",
                PURCHASE.to_owned(),
                cookie.as_str(),
                json!({"reward_id":unowned["id"],"revision":unowned["revision"]}),
            ),
            (
                "PATCH",
                EQUIPMENT.to_owned(),
                cookie.as_str(),
                json!({"title_id":title["id"],"avatar_frame_id":null}),
            ),
        ] {
            let failed = request(&app, method, &path, Some(cookie), body).await;
            assert_eq!(
                failed.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                "{method} {path}: {}",
                failed.1
            );
            assert_eq!(
                snapshot(&state.db).await,
                before,
                "{method} {path} must rollback all reward/accounting/audit rows"
            );
        }
    })
    .await;
    fixture.finish(result).await;
}
