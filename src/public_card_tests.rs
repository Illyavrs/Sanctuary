//! Public hover-card and avatar-decoration lookups use an owned PG17 schema.
use super::{CardQuery, card_ids};
use crate::{
    Router,
    maps::map_mutation_tests::{MutationFixture, actor},
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

#[test]
fn public_card_ids_are_positive_bounded_and_deduplicated_in_order() {
    let parse = |ids: Option<&str>| {
        card_ids(CardQuery {
            ids: ids.map(str::to_owned),
        })
    };
    assert_eq!(parse(Some("3,1,3,02,2")).unwrap(), vec![3, 1, 2]);
    assert_eq!(parse(Some("9223372036854775807")).unwrap(), vec![i64::MAX]);
    let sixty = (1..=60)
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(parse(Some(&sixty)).unwrap(), (1..=60).collect::<Vec<_>>());
    for invalid in [
        None,
        Some(""),
        Some("0"),
        Some("-1"),
        Some("+1"),
        Some("1,"),
        Some(",1"),
        Some("1,,2"),
        Some("1, 2"),
        Some("1.0"),
        Some("1e2"),
        Some("١"),
        Some("9223372036854775808"),
        Some("00000000000000000001"),
    ] {
        assert_eq!(parse(invalid).unwrap_err().0, StatusCode::BAD_REQUEST);
    }
    let repeated = vec!["1"; 61].join(",");
    assert_eq!(
        parse(Some(&repeated)).unwrap_err().0,
        StatusCode::BAD_REQUEST
    );
}

async fn get(app: &Router, path: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
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

fn assert_public_card(card: &Value, user: i64, cosmetics: &Value) {
    assert_eq!(card["id"], user);
    assert_eq!(card["cosmetics"], *cosmetics);
    let fields = [
        "id",
        "username",
        "avatar_url",
        "country",
        "banner_url",
        "banner_position_x",
        "banner_position_y",
        "accent_color",
        "online",
        "groups",
        "cosmetics",
    ];
    let object = card.as_object().unwrap();
    assert_eq!(
        object.len(),
        fields.len(),
        "private or unexpected card field"
    );
    assert!(fields.iter().all(|field| object.contains_key(*field)));
    let equipment = card["cosmetics"].as_object().unwrap();
    assert_eq!(equipment.len(), 2);
    for key in ["title", "avatar_frame"] {
        if let Some(reward) = equipment[key].as_object() {
            let fields = [
                "id",
                "name",
                "description",
                "kind",
                "price",
                "color",
                "style",
                "active",
                "revision",
                "image_url",
            ];
            assert_eq!(reward.len(), fields.len());
            assert!(fields.iter().all(|field| reward.contains_key(*field)));
        }
    }
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn public_card_lookup_matches_profile_cosmetics_without_private_state() {
    let fixture = MutationFixture::new("public_card", "http://127.0.0.1:1".into()).await;
    let state = fixture.state.clone();
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        let (owner, role, _, _) = actor(&state, "PublicCardOwner", &[]).await;
        let (bare, _, _, _) = actor(&state, "PublicCardBare", &[]).await;
        let (restricted, _, _, _) = actor(&state, "PublicCardRestricted", &[]).await;
        sqlx::query(
            "UPDATE roles SET display_badge=true,short_name='CARD',colour='#44aaff' WHERE id=$1",
        )
        .bind(role)
        .execute(&state.db)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE users SET country='TW',avatar_url='/media/avatars/fixture.png',\
             banner_url='/media/banners/fixture.png',accent_color='#44aaff' WHERE id=$1",
        )
        .bind(owner)
        .execute(&state.db)
        .await
        .unwrap();
        sqlx::query("UPDATE users SET is_restricted=true WHERE id=$1")
            .bind(restricted)
            .execute(&state.db)
            .await
            .unwrap();
        let title: i64 = sqlx::query_scalar(
            "INSERT INTO reward_catalog(name,description,kind,price,active) \
             VALUES('Card equipped title','Public catalog metadata','title',15,false) RETURNING id",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        let image_url = format!("/media/reward-frames/{}.png", Uuid::new_v4());
        let frame: i64 = sqlx::query_scalar(
            "INSERT INTO reward_catalog(name,kind,price,color,style,active,image_url) \
             VALUES('Card image frame','avatar_frame',25,'#44aaff','image',false,$1) RETURNING id",
        )
        .bind(&image_url)
        .fetch_one(&state.db)
        .await
        .unwrap();
        let hidden: i64 = sqlx::query_scalar(
            "INSERT INTO reward_catalog(name,kind,price,active) \
             VALUES('Private unequipped inventory','title',5,true) RETURNING id",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        for user in [owner, restricted] {
            for reward in [title, frame, hidden] {
                sqlx::query("INSERT INTO reward_inventory(user_id,reward_id,paid) VALUES($1,$2,5)")
                    .bind(user)
                    .bind(reward)
                    .execute(&state.db)
                    .await
                    .unwrap();
            }
            sqlx::query(
                "INSERT INTO reward_equipment(user_id,title_id,avatar_frame_id) VALUES($1,$2,$3)",
            )
            .bind(user)
            .bind(title)
            .bind(frame)
            .execute(&state.db)
            .await
            .unwrap();
            sqlx::query("INSERT INTO reward_wallets(user_id,balance) VALUES($1,12345)")
                .bind(user)
                .execute(&state.db)
                .await
                .unwrap();
        }
        let profile = get(&app, &format!("/api/v1/users/{owner}")).await;
        assert_eq!(profile.0, StatusCode::OK, "{}", profile.1);
        let cosmetics = &profile.1["cosmetics"];
        assert_eq!(cosmetics["title"]["id"], title);
        assert_eq!(cosmetics["avatar_frame"]["id"], frame);
        assert_eq!(cosmetics["avatar_frame"]["image_url"], image_url);
        assert_eq!(cosmetics["avatar_frame"]["active"], false);
        let card = get(&app, &format!("/api/v1/users/{owner}/card")).await;
        assert_eq!(card.0, StatusCode::OK, "{}", card.1);
        assert_public_card(&card.1, owner, cosmetics);
        assert_eq!(card.1["country"], "TW");
        assert_eq!(card.1["avatar_url"], "/media/avatars/fixture.png");
        assert_eq!(card.1["accent_color"], "#44aaff");
        assert_eq!(card.1["online"], false);
        assert_eq!(card.1["groups"][0]["id"], role);
        assert_eq!(card.1["groups"][0]["short_name"], "CARD");
        assert!(!card.1.to_string().contains("Private unequipped inventory"));

        // The batch contains only the requested visible users, deduplicated in
        // request order. An equipped delisted frame remains public metadata.
        let batch = get(
            &app,
            &format!(
                "/api/v1/users/cards?ids={bare},{owner},{bare},{restricted},9223372036854775807"
            ),
        )
        .await;
        assert_eq!(batch.0, StatusCode::OK, "{}", batch.1);
        assert_eq!(batch.1.as_object().unwrap().len(), 1);
        let items = batch.1["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_public_card(&items[0], bare, &json!({"title":null,"avatar_frame":null}));
        assert_eq!(items[1], card.1);
        let invisible = get(&app, &format!("/api/v1/users/{restricted}/card")).await;
        let missing = get(&app, "/api/v1/users/9223372036854775807/card").await;
        assert_eq!(invisible.0, StatusCode::NOT_FOUND);
        assert_eq!(invisible, missing);
        let none = get(
            &app,
            &format!("/api/v1/users/cards?ids={restricted},9223372036854775807"),
        )
        .await;
        assert_eq!(none, (StatusCode::OK, json!({"items":[]})));

        sqlx::query(
            "UPDATE reward_equipment SET title_id=NULL,avatar_frame_id=NULL WHERE user_id=$1",
        )
        .bind(owner)
        .execute(&state.db)
        .await
        .unwrap();
        let unset = get(&app, &format!("/api/v1/users/{owner}/card")).await;
        assert_eq!(unset.0, StatusCode::OK);
        assert_public_card(&unset.1, owner, &json!({"title":null,"avatar_frame":null}));
        let unset_profile = get(&app, &format!("/api/v1/users/{owner}")).await;
        assert_eq!(unset.1["cosmetics"], unset_profile.1["cosmetics"]);
        sqlx::query("UPDATE users SET is_restricted=true WHERE id=$1")
            .bind(owner)
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(
            get(&app, &format!("/api/v1/users/{owner}/card")).await,
            missing
        );
        let batch = get(&app, &format!("/api/v1/users/cards?ids={owner},{bare}")).await;
        assert_eq!(batch.0, StatusCode::OK);
        assert_eq!(batch.1["items"].as_array().unwrap().len(), 1);
        assert_eq!(batch.1["items"][0]["id"], bare);
        let remaining: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT balance FROM reward_wallets WHERE user_id=$1),\
             (SELECT count(*) FROM reward_inventory WHERE user_id=$1),\
             (SELECT count(*) FROM reward_ledger WHERE user_id=$1)",
        )
        .bind(owner)
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(
            remaining,
            (12345, 3, 0),
            "public reads cannot claim or spend rewards"
        );
    })
    .await;
    fixture.finish(result).await;
}

#[tokio::test]
#[ignore = "requires ILLYA_TEST_DATABASE_URL; owned disposable PostgreSQL 17"]
async fn public_card_http_bounds_reject_invalid_queries_before_database_lookups() {
    let fixture = MutationFixture::new("public_card_bounds", "http://127.0.0.1:1".into()).await;
    let app = fixture.app.clone();
    let result = tokio::spawn(async move {
        for path in [
            "/api/v1/users/0/card",
            "/api/v1/users/-1/card",
            "/api/v1/users/cards",
            "/api/v1/users/cards?ids=",
            "/api/v1/users/cards?ids=0",
            "/api/v1/users/cards?ids=-1",
            "/api/v1/users/cards?ids=1,",
            "/api/v1/users/cards?ids=1,,2",
            "/api/v1/users/cards?ids=1,%202",
            "/api/v1/users/cards?ids=1.5",
            "/api/v1/users/cards?ids=9223372036854775808",
            "/api/v1/users/cards?ids=1&balance=true",
            "/api/v1/users/cards?ids=1&ids=2",
        ] {
            let (status, body) = get(&app, path).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        }
        let too_many = vec!["1"; 61].join(",");
        assert_eq!(
            get(&app, &format!("/api/v1/users/cards?ids={too_many}"))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        let maximum = (1..=60)
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let allowed = get(&app, &format!("/api/v1/users/cards?ids={maximum}")).await;
        assert_eq!(allowed.0, StatusCode::OK, "{}", allowed.1);
        assert!(allowed.1["items"].as_array().unwrap().len() <= 60);
    })
    .await;
    fixture.finish(result).await;
}
