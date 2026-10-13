//! Weekly exploration rewards. Every mutation fences authorization before
//! wallet/catalog locks and commits balance, ownership, ledger and audit together.
use crate::{ApiError, ApiResult, AppState, auth, page};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::HeaderMap,
    http::StatusCode,
    routing::{get, patch, post},
};
use chrono::{DateTime, Datelike, Duration, FixedOffset, SecondsFormat, TimeZone, Utc};
use illya_domain::{Pagination, validate_color};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

type Tx<'a> = Transaction<'a, Postgres>;
const REWARD: &str = "jsonb_build_object('id',r.id,'name',r.name,'description',r.description,\
    'kind',r.kind,'price',r.price,'color',r.color,'style',r.style,'image_url',r.image_url,'active',r.active,'revision',r.revision)";
const MISSIONS: [(&str, i64); 3] = [("play_5", 5), ("maps_3", 3), ("variety_2", 2)];
const MISSION_REWARD: i64 = 20;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/rewards", get(list))
        .route("/api/v1/me/rewards", get(me))
        .route("/api/v1/me/rewards/missions/{key}/claim", post(claim))
        .route("/api/v1/me/rewards/purchases", post(purchase))
        .route("/api/v1/me/rewards/equipment", patch(equip))
        .route("/api/v1/admin/rewards", get(admin_list).post(create))
        .route("/api/v1/admin/rewards/{id}", patch(update).delete(remove))
        .route(
            "/api/v1/admin/rewards/{id}/image",
            post(crate::reward_frame_media::upload)
                .layer(DefaultBodyLimit::max(2 * 1024 * 1024 + 16 * 1024)),
        )
}

#[derive(Deserialize)]
struct ListQuery {
    page: Option<u32>,
    page_size: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RewardInput {
    name: String,
    description: String,
    kind: String,
    price: i64,
    color: Option<String>,
    style: Option<String>,
    active: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateInput {
    revision: i64,
    name: String,
    description: String,
    kind: String,
    price: i64,
    color: Option<String>,
    style: Option<String>,
    active: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisionInput {
    revision: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimInput {
    week_start: DateTime<Utc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PurchaseInput {
    reward_id: i64,
    revision: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EquipmentInput {
    // deserialize_with without a default rejects omission but accepts null.
    #[serde(deserialize_with = "nullable_id")]
    title_id: Option<i64>,
    #[serde(deserialize_with = "nullable_id")]
    avatar_frame_id: Option<i64>,
}

fn nullable_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<i64>, D::Error> {
    Option::<i64>::deserialize(deserializer)
}

fn validate_reward(mut input: RewardInput) -> ApiResult<RewardInput> {
    if input.name.chars().any(char::is_control) {
        return Err(ApiError::bad(
            "Reward names cannot contain control characters.",
        ));
    }
    input.name = input.name.trim().to_owned();
    if !(1..=32).contains(&input.name.chars().count()) {
        return Err(ApiError::bad("Reward names must be 1–32 characters."));
    }
    input.description = input.description.replace("\r\n", "\n");
    if input.description.chars().count() > 300
        || input
            .description
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(ApiError::bad(
            "Descriptions must be at most 300 plain-text characters.",
        ));
    }
    if !(1..=100000).contains(&input.price) {
        return Err(ApiError::bad("Reward prices must be 1–100000."));
    }
    if input
        .color
        .as_deref()
        .is_some_and(|color| !validate_color(color))
    {
        return Err(ApiError::bad("Colors must be null or #RRGGBB."));
    }
    let valid_style = match input.kind.as_str() {
        "title" => input.style.is_none(),
        "avatar_frame" => matches!(
            input.style.as_deref(),
            Some("line" | "double" | "corners" | "image")
        ),
        _ => false,
    };
    if !valid_style {
        return Err(ApiError::bad(
            "Titles have no style; avatar frames use line, double, corners or image.",
        ));
    }
    Ok(input)
}

pub(crate) fn positive(value: i64) -> ApiResult<()> {
    if value > 0 {
        Ok(())
    } else {
        Err(ApiError::bad("IDs and revisions must be positive."))
    }
}

pub(crate) fn conflict(code: &'static str, message: &str) -> ApiError {
    ApiError(StatusCode::CONFLICT, code, message.into())
}

pub(crate) fn revision_matches(expected: i64, actual: i64) -> ApiResult<()> {
    if expected == actual {
        Ok(())
    } else {
        Err(conflict(
            "reward_conflict",
            "This reward changed. Reload it before continuing.",
        ))
    }
}

#[derive(Clone, Copy)]
struct Week {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

fn week_at(now: DateTime<Utc>) -> Week {
    // Taipei has no daylight-saving transitions. Monday 00:00 local is Sunday 16:00 UTC.
    let taipei = FixedOffset::east_opt(8 * 3600).expect("valid Taipei offset");
    let local = now.with_timezone(&taipei);
    let monday =
        local.date_naive() - Duration::days(i64::from(local.weekday().num_days_from_monday()));
    let start = taipei
        .from_local_datetime(&monday.and_hms_opt(0, 0, 0).expect("midnight"))
        .single()
        .expect("fixed-offset local time")
        .with_timezone(&Utc);
    Week {
        start,
        end: start + Duration::days(7),
    }
}

async fn current_week(tx: &mut Tx<'_>) -> ApiResult<Week> {
    let now = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    Ok(week_at(now))
}

async fn read_tx(state: &AppState) -> ApiResult<Tx<'static>> {
    let mut tx = state.db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

async fn progress(tx: &mut Tx<'_>, user_id: i64, week: Week) -> ApiResult<[i64; 3]> {
    let (plays, maps, variety): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*),count(DISTINCT beatmap_id),count(DISTINCT (mode,score_variant)) \
         FROM scores WHERE user_id=$1 AND verification='verified' AND passed \
         AND created_at >= $2 AND created_at < $3 \
         AND (submission_origin <> 'replay' OR (replay_played_at >= $2 AND replay_played_at < $3))",
    )
    .bind(user_id)
    .bind(week.start)
    .bind(week.end)
    .fetch_one(&mut **tx)
    .await?;
    Ok([plays.min(5), maps.min(3), variety.min(2)])
}

fn equipment_sql() -> String {
    format!(
        "SELECT jsonb_build_object('title',\
         (SELECT {REWARD} FROM reward_equipment e JOIN reward_catalog r ON r.id=e.title_id \
          WHERE e.user_id=$1 AND r.kind='title'),'avatar_frame',\
         (SELECT {REWARD} FROM reward_equipment e JOIN reward_catalog r ON r.id=e.avatar_frame_id \
          WHERE e.user_id=$1 AND r.kind='avatar_frame'))"
    )
}

/// Public profile decoration projection; never expose balance, missions or inventory.
pub async fn profile_equipment(state: &AppState, user_id: i64) -> ApiResult<Value> {
    Ok(sqlx::query_scalar::<_, Value>(&equipment_sql())
        .bind(user_id)
        .fetch_one(&state.db)
        .await?)
}

async fn player_state(tx: &mut Tx<'_>, user_id: i64, week: Week) -> ApiResult<Value> {
    // One statement projects every field from the same snapshot, including
    // catalog metadata that other administrators may edit during this mutation.
    let equipment = equipment_sql();
    Ok(sqlx::query_scalar::<_, Value>(&format!(
        r#"WITH eligible AS (
            SELECT count(*) AS plays,count(DISTINCT beatmap_id) AS maps,
                count(DISTINCT (mode,score_variant)) AS variety
            FROM scores WHERE user_id=$1 AND verification='verified' AND passed
                AND created_at >= $2 AND created_at < $3
                AND (submission_origin <> 'replay' OR (replay_played_at >= $2 AND replay_played_at < $3))
        )
        SELECT jsonb_build_object(
            'balance',COALESCE((SELECT balance FROM reward_wallets WHERE user_id=$1),0),
            'week_start',$4::text,'resets_at',$5::text,
            'missions',(SELECT jsonb_agg(jsonb_build_object(
                'key',m.key,'goal',m.goal,'progress',LEAST(m.progress,m.goal),'reward',20,
                'claimed',EXISTS(SELECT 1 FROM reward_mission_claims c
                    WHERE c.user_id=$1 AND c.week_start=$2 AND c.mission_key=m.key)
                ) ORDER BY m.n)
                FROM eligible p CROSS JOIN LATERAL
                    (VALUES (0,'play_5',5,p.plays),(1,'maps_3',3,p.maps),(2,'variety_2',2,p.variety))
                    AS m(n,key,goal,progress)),
            'inventory',COALESCE((SELECT jsonb_agg({REWARD} || jsonb_build_object(
                'acquired_at',to_char(i.acquired_at AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
                'paid',i.paid) ORDER BY i.acquired_at,i.reward_id)
                FROM reward_inventory i JOIN reward_catalog r ON r.id=i.reward_id
                WHERE i.user_id=$1),'[]'::jsonb),
            'equipment',({equipment})
        )"#
    ))
    .bind(user_id)
    .bind(week.start)
    .bind(week.end)
    .bind(week.start.to_rfc3339_opts(SecondsFormat::Secs, true))
    .bind(week.end.to_rfc3339_opts(SecondsFormat::Secs, true))
    .fetch_one(&mut **tx)
    .await?)
}

async fn lock_wallet(tx: &mut Tx<'_>, user_id: i64) -> ApiResult<i64> {
    // mutation_actor's permission/user/session fences must already be held.
    sqlx::query("INSERT INTO reward_wallets(user_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    Ok(
        sqlx::query_scalar("SELECT balance FROM reward_wallets WHERE user_id=$1 FOR UPDATE")
            .bind(user_id)
            .fetch_one(&mut **tx)
            .await?,
    )
}

pub(crate) async fn audit_in(
    tx: &mut Tx<'_>,
    actor: i64,
    action: &str,
    target_type: &str,
    target: impl ToString,
    details: Value,
) -> ApiResult<()> {
    sqlx::query("INSERT INTO audit_events(actor_id,action,target_type,target_id,details) VALUES($1,$2,$3,$4,$5)")
        .bind(actor).bind(action).bind(target_type).bind(target.to_string()).bind(details)
        .execute(&mut **tx).await?;
    Ok(())
}

async fn mutation_response(
    mut tx: Tx<'_>,
    actor: &auth::MutationActor,
    idempotent: bool,
) -> ApiResult {
    let week = current_week(&mut tx).await?;
    let state = player_state(&mut tx, actor.id, week).await?;
    auth::ensure_mutation_actor(&mut tx, actor).await?;
    tx.commit().await?;
    Ok(Json(json!({"state":state,"idempotent":idempotent})))
}

async fn list(State(state): State<AppState>, Query(query): Query<ListQuery>) -> ApiResult {
    let (p, size, offset) = Pagination {
        page: query.page,
        page_size: query.page_size,
    }
    .bounds();
    let mut tx = read_tx(&state).await?;
    let total = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM reward_catalog WHERE active")
        .fetch_one(&mut *tx)
        .await?;
    let items = sqlx::query_scalar::<_, Value>(&format!(
        "SELECT {REWARD} FROM reward_catalog r WHERE active ORDER BY kind,price,id LIMIT $1 OFFSET $2"
    )).bind(size).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(page(items, total, p, size))
}

async fn me(State(state): State<AppState>, headers: HeaderMap) -> ApiResult {
    let user = auth::require(&state, &headers).await?;
    let mut tx = read_tx(&state).await?;
    let week = current_week(&mut tx).await?;
    let result = player_state(&mut tx, user.id, week).await?;
    tx.commit().await?;
    Ok(Json(result))
}

async fn claim(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Json(input): Json<ClaimInput>,
) -> ApiResult {
    let snapshot = auth::require(&state, &headers).await?;
    let index = MISSIONS
        .iter()
        .position(|(name, _)| *name == key)
        .ok_or_else(|| ApiError::bad("Unknown mission key."))?;
    let mut tx = state.db.begin().await?;
    let actor = auth::mutation_actor(&mut tx, &snapshot, &[], auth::MutationScope::Session).await?;
    let balance = lock_wallet(&mut tx, actor.id).await?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    // A committed old-week receipt remains retryable after rollover; new claims
    // require the displayed current week and can never backfill a prior week.
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM reward_mission_claims WHERE user_id=$1 AND week_start=$2 AND mission_key=$3)"
    ).bind(actor.id).bind(input.week_start).bind(&key).fetch_one(&mut *tx).await?;
    if exists {
        return mutation_response(tx, &actor, true).await;
    }
    let week = current_week(&mut tx).await?;
    if input.week_start != week.start {
        return Err(conflict(
            "mission_week_changed",
            "The mission week changed. Reload your rewards.",
        ));
    }
    if progress(&mut tx, actor.id, week).await?[index] < MISSIONS[index].1 {
        return Err(conflict(
            "mission_incomplete",
            "Complete this mission before claiming its reward.",
        ));
    }
    let next_balance = balance
        .checked_add(MISSION_REWARD)
        .ok_or_else(|| ApiError::bad("The wallet balance is exhausted."))?;
    sqlx::query(
        "INSERT INTO reward_mission_claims(user_id,week_start,mission_key) VALUES($1,$2,$3)",
    )
    .bind(actor.id)
    .bind(week.start)
    .bind(&key)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE reward_wallets SET balance=$2,updated_at=clock_timestamp() WHERE user_id=$1",
    )
    .bind(actor.id)
    .bind(next_balance)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO reward_ledger(user_id,delta,balance_after,event_type,mission_key,week_start) VALUES($1,20,$2,'mission',$3,$4)")
        .bind(actor.id).bind(next_balance).bind(&key).bind(week.start).execute(&mut *tx).await?;
    audit_in(
        &mut tx,
        actor.id,
        "rewards.mission_claim",
        "reward_mission",
        &key,
        json!({"week_start":week.start,"delta":MISSION_REWARD,"balance_after":next_balance}),
    )
    .await?;
    mutation_response(tx, &actor, false).await
}

async fn purchase(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PurchaseInput>,
) -> ApiResult {
    let snapshot = auth::require(&state, &headers).await?;
    positive(input.reward_id)?;
    positive(input.revision)?;
    let mut tx = state.db.begin().await?;
    let actor = auth::mutation_actor(&mut tx, &snapshot, &[], auth::MutationScope::Session).await?;
    let balance = lock_wallet(&mut tx, actor.id).await?;
    // Catalog locks follow the wallet and never lead back to any player lock.
    let (price, active, revision, ready): (i64, bool, i64, bool) =
        sqlx::query_as("SELECT price,active,revision,(style IS DISTINCT FROM 'image' OR image_url IS NOT NULL) FROM reward_catalog WHERE id=$1 FOR UPDATE")
            .bind(input.reward_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(ApiError::missing)?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM reward_inventory WHERE user_id=$1 AND reward_id=$2)",
    )
    .bind(actor.id)
    .bind(input.reward_id)
    .fetch_one(&mut *tx)
    .await?;
    // Retrying a committed purchase succeeds even if the administrator has
    // subsequently changed the revision, price, or availability.
    if owned {
        return mutation_response(tx, &actor, true).await;
    }
    revision_matches(input.revision, revision)?;
    if !active || !ready {
        return Err(conflict(
            "reward_unavailable",
            "This reward is no longer available.",
        ));
    }
    if balance < price {
        return Err(conflict(
            "insufficient_stardust",
            "You do not have enough stardust.",
        ));
    }
    let next_balance = balance - price;
    sqlx::query(
        "UPDATE reward_wallets SET balance=$2,updated_at=clock_timestamp() WHERE user_id=$1",
    )
    .bind(actor.id)
    .bind(next_balance)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO reward_inventory(user_id,reward_id,paid) VALUES($1,$2,$3)")
        .bind(actor.id)
        .bind(input.reward_id)
        .bind(price)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO reward_ledger(user_id,delta,balance_after,event_type,reward_id) VALUES($1,$2,$3,'purchase',$4)")
        .bind(actor.id).bind(-price).bind(next_balance).bind(input.reward_id).execute(&mut *tx).await?;
    audit_in(
        &mut tx,
        actor.id,
        "rewards.purchase",
        "reward",
        input.reward_id,
        json!({"revision":revision,"paid":price,"delta":-price,"balance_after":next_balance}),
    )
    .await?;
    mutation_response(tx, &actor, false).await
}

async fn equip(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<EquipmentInput>,
) -> ApiResult {
    let snapshot = auth::require(&state, &headers).await?;
    let mut ids = input
        .title_id
        .into_iter()
        .chain(input.avatar_frame_id)
        .collect::<Vec<_>>();
    for id in &ids {
        positive(*id)?;
    }
    ids.sort_unstable();
    ids.dedup();
    let mut tx = state.db.begin().await?;
    let actor = auth::mutation_actor(&mut tx, &snapshot, &[], auth::MutationScope::Session).await?;
    lock_wallet(&mut tx, actor.id).await?;
    // Multi-reward operations lock sorted IDs so simultaneous equipment/admin
    // edits cannot acquire the same catalog rows in opposite orders.
    let records: Vec<(i64, String, bool)> = sqlx::query_as(
        "SELECT id,kind,(style IS DISTINCT FROM 'image' OR image_url IS NOT NULL) FROM reward_catalog WHERE id=ANY($1) ORDER BY id FOR UPDATE",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    let owned = sqlx::query_scalar::<_, i64>(
        "SELECT reward_id FROM reward_inventory WHERE user_id=$1 AND reward_id=ANY($2)",
    )
    .bind(actor.id)
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?;
    if ids.iter().any(|id| !owned.contains(id)) {
        return Err(conflict(
            "reward_not_owned",
            "Equip only rewards from your own inventory.",
        ));
    }
    for (id, expected) in [
        (input.title_id, "title"),
        (input.avatar_frame_id, "avatar_frame"),
    ] {
        if let Some(id) = id
            && !records
                .iter()
                .any(|(reward_id, kind, _)| *reward_id == id && kind == expected)
        {
            return Err(ApiError::bad(
                "That reward does not match the equipment slot.",
            ));
        }
    }
    if records.iter().any(|(_, _, ready)| !ready) {
        return Err(conflict(
            "reward_image_required",
            "Upload an image before equipping this avatar frame.",
        ));
    }
    // Availability is deliberately irrelevant for existing owners.
    sqlx::query(
        "INSERT INTO reward_equipment(user_id,title_id,avatar_frame_id) VALUES($1,$2,$3) \
         ON CONFLICT(user_id) DO UPDATE SET title_id=EXCLUDED.title_id,avatar_frame_id=EXCLUDED.avatar_frame_id"
    ).bind(actor.id).bind(input.title_id).bind(input.avatar_frame_id).execute(&mut *tx).await?;
    audit_in(
        &mut tx,
        actor.id,
        "rewards.equip",
        "user",
        actor.id,
        json!({"title_id":input.title_id,"avatar_frame_id":input.avatar_frame_id}),
    )
    .await?;
    mutation_response(tx, &actor, false).await
}

fn admin_record_sql() -> String {
    format!(
        r#"{REWARD} || jsonb_build_object('owned_count',
        (SELECT count(*) FROM reward_inventory i WHERE i.reward_id=r.id),
        'created_at',to_char(r.created_at AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
        'updated_at',to_char(r.updated_at AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS.US"Z"'))"#
    )
}

pub(crate) async fn admin_record(tx: &mut Tx<'_>, id: i64) -> ApiResult<Value> {
    sqlx::query_scalar::<_, Value>(&format!(
        "SELECT {} FROM reward_catalog r WHERE r.id=$1",
        admin_record_sql()
    ))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(ApiError::missing)
}

async fn admin_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> ApiResult {
    let user = auth::require(&state, &headers).await?;
    user.require("rewards.manage")?;
    let (p, size, offset) = Pagination {
        page: query.page,
        page_size: query.page_size,
    }
    .bounds();
    let mut tx = read_tx(&state).await?;
    let total = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM reward_catalog")
        .fetch_one(&mut *tx)
        .await?;
    let items = sqlx::query_scalar::<_, Value>(&format!(
        "SELECT {} FROM reward_catalog r ORDER BY updated_at DESC,id DESC LIMIT $1 OFFSET $2",
        admin_record_sql()
    ))
    .bind(size)
    .bind(offset)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(page(items, total, p, size))
}

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<RewardInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let snapshot = auth::require(&state, &headers).await?;
    snapshot.require("rewards.manage")?;
    let input = validate_reward(input)?;
    if input.style.as_deref() == Some("image") && input.active {
        return Err(conflict(
            "reward_image_required",
            "Save the frame as a draft, upload its image, then publish it.",
        ));
    }
    let mut tx = state.db.begin().await?;
    let actor =
        auth::mutation_actor(&mut tx, &snapshot, &[], auth::MutationScope::Permissions).await?;
    actor.require("rewards.manage")?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO reward_catalog(name,description,kind,price,color,style,active) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id"
    ).bind(input.name).bind(input.description).bind(input.kind).bind(input.price)
        .bind(input.color).bind(input.style).bind(input.active).fetch_one(&mut *tx).await?;
    let item = admin_record(&mut tx, id).await?;
    audit_in(
        &mut tx,
        actor.id,
        "rewards.create",
        "reward",
        id,
        json!({"reward":item}),
    )
    .await?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(item)))
}

async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<UpdateInput>,
) -> ApiResult {
    let snapshot = auth::require(&state, &headers).await?;
    snapshot.require("rewards.manage")?;
    positive(id)?;
    positive(input.revision)?;
    let expected_revision = input.revision;
    let input = validate_reward(RewardInput {
        name: input.name,
        description: input.description,
        kind: input.kind,
        price: input.price,
        color: input.color,
        style: input.style,
        active: input.active,
    })?;
    let mut tx = state.db.begin().await?;
    let actor =
        auth::mutation_actor(&mut tx, &snapshot, &[], auth::MutationScope::Permissions).await?;
    actor.require("rewards.manage")?;
    let (revision, kind): (i64, String) =
        sqlx::query_as("SELECT revision,kind FROM reward_catalog WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(ApiError::missing)?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    revision_matches(expected_revision, revision)?;
    let before = admin_record(&mut tx, id).await?;
    if kind != input.kind && before["owned_count"].as_i64().unwrap_or(0) > 0 {
        return Err(conflict(
            "reward_in_use",
            "Owned rewards cannot change kind. You can take this reward off sale.",
        ));
    }
    let image_url = if input.kind == "avatar_frame" && input.style.as_deref() == Some("image") {
        before["image_url"].as_str()
    } else {
        None
    };
    if input.style.as_deref() == Some("image")
        && image_url.is_none()
        && (input.active || before["owned_count"].as_i64().unwrap_or(0) > 0)
    {
        return Err(conflict(
            "reward_image_required",
            "Upload the frame image before publishing or changing an owned frame to image style.",
        ));
    }
    let next = revision
        .checked_add(1)
        .ok_or_else(|| ApiError::bad("The reward revision is exhausted."))?;
    sqlx::query(
        "UPDATE reward_catalog SET name=$2,description=$3,kind=$4,price=$5,color=$6,style=$7,active=$8,\
         revision=$9,image_url=$10,updated_at=clock_timestamp() WHERE id=$1"
    ).bind(id).bind(input.name).bind(input.description).bind(input.kind).bind(input.price)
        .bind(input.color).bind(input.style).bind(input.active).bind(next).bind(image_url).execute(&mut *tx).await?;
    let item = admin_record(&mut tx, id).await?;
    audit_in(
        &mut tx,
        actor.id,
        "rewards.update",
        "reward",
        id,
        json!({"before":before,"after":item}),
    )
    .await?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    tx.commit().await?;
    Ok(Json(item))
}

async fn remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<RevisionInput>,
) -> ApiResult {
    let snapshot = auth::require(&state, &headers).await?;
    snapshot.require("rewards.manage")?;
    positive(id)?;
    positive(input.revision)?;
    let mut tx = state.db.begin().await?;
    let actor =
        auth::mutation_actor(&mut tx, &snapshot, &[], auth::MutationScope::Permissions).await?;
    actor.require("rewards.manage")?;
    let revision =
        sqlx::query_scalar::<_, i64>("SELECT revision FROM reward_catalog WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(ApiError::missing)?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    revision_matches(input.revision, revision)?;
    let before = admin_record(&mut tx, id).await?;
    if before["owned_count"].as_i64().unwrap_or(0) > 0 {
        return Err(conflict(
            "reward_in_use",
            "Players own this reward. Take it off sale instead of deleting it.",
        ));
    }
    sqlx::query("DELETE FROM reward_catalog WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit_in(
        &mut tx,
        actor.id,
        "rewards.delete",
        "reward",
        id,
        json!({"reward":before}),
    )
    .await?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    tx.commit().await?;
    Ok(Json(json!({"ok":true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> RewardInput {
        RewardInput {
            name: "探索旅人".into(),
            description: String::new(),
            kind: "title".into(),
            price: 40,
            color: None,
            style: None,
            active: false,
        }
    }

    #[test]
    fn equipment_requires_both_explicit_nullable_fields() {
        for value in [
            json!({}),
            json!({"title_id":null}),
            json!({"avatar_frame_id":null}),
            json!({"title_id":null,"avatar_frame_id":null,"user_id":42}),
        ] {
            assert!(serde_json::from_value::<EquipmentInput>(value).is_err());
        }
        assert!(
            serde_json::from_value::<EquipmentInput>(
                json!({"title_id":null,"avatar_frame_id":null})
            )
            .is_ok()
        );
    }

    #[test]
    fn catalog_validation_counts_unicode_and_restricts_css_values() {
        let mut value = input();
        value.name = "花".repeat(32);
        assert!(validate_reward(value).is_ok());
        let mut value = input();
        value.name = "花".repeat(33);
        assert!(validate_reward(value).is_err());
        for color in ["red", "#abc", "#12345678", "url(https://example.test)"] {
            let mut value = input();
            value.color = Some(color.into());
            assert!(validate_reward(value).is_err());
        }
        let mut value = input();
        value.color = Some("#aB09Ef".into());
        assert!(validate_reward(value).is_ok());
        for style in ["line", "double", "corners"] {
            let mut value = input();
            value.style = Some(style.into());
            assert!(validate_reward(value).is_err());
            let mut value = input();
            value.kind = "avatar_frame".into();
            value.style = Some(style.into());
            assert!(validate_reward(value).is_ok());
        }
        let mut value = input();
        value.kind = "avatar_frame".into();
        assert!(validate_reward(value).is_err());
        let mut value = input();
        value.kind = "avatar_frame".into();
        value.style = Some("animation".into());
        assert!(validate_reward(value).is_err());
    }

    #[test]
    fn plain_text_and_price_limits() {
        for name in ["", "   ", "title\n", "name\u{7f}"] {
            let mut value = input();
            value.name = name.into();
            assert!(validate_reward(value).is_err());
        }
        for price in [0, -1, 100001] {
            let mut value = input();
            value.price = price;
            assert!(validate_reward(value).is_err());
        }
        let mut value = input();
        value.description = "花".repeat(300);
        assert!(validate_reward(value).is_ok());
        let mut value = input();
        value.description = "花".repeat(301);
        assert!(validate_reward(value).is_err());
        let mut value = input();
        value.description = "plain\r\ntext\t".into();
        assert_eq!(validate_reward(value).unwrap().description, "plain\ntext\t");
        let mut value = input();
        value.description = "hidden\u{8}text".into();
        assert!(validate_reward(value).is_err());
    }

    #[test]
    fn taipei_week_rolls_over_on_sunday_at_1600_utc() {
        let before: DateTime<Utc> = "2026-10-11T15:59:59Z".parse().unwrap();
        let after: DateTime<Utc> = "2026-10-11T16:00:00Z".parse().unwrap();
        let week = week_at(before);
        assert_eq!(
            week.start.to_rfc3339_opts(SecondsFormat::Secs, true),
            "2026-10-04T16:00:00Z"
        );
        assert_eq!(week.end, after);
        assert_eq!(week_at(after).start, after);
        assert_eq!(week_at(after).end - week_at(after).start, Duration::days(7));
    }
}
