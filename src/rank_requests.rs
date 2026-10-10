//! Private, versioned player nominations and set-consistent staff decisions.
use crate::{
    ApiError, ApiResult, AppState, auth, beatmap_availability as availability, maps, page,
    server_catalogue,
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use illya_domain::{Pagination, has_permission};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;

const LOCAL_SET_MIN: i64 = 2_000_000_000;
const MAX_SET_ID: i64 = i32::MAX as i64;
const MAX_OPEN_REQUESTS: i64 = 5;
const MAX_DAILY_REQUESTS: i64 = 10;

#[cfg(test)]
#[path = "rank_request_tests.rs"]
mod tests;

fn error(status: StatusCode, code: &'static str, message: &str) -> ApiError {
    ApiError(status, code, message.into())
}
fn ineligible() -> ApiError {
    error(
        StatusCode::BAD_REQUEST,
        "rank_request_ineligible",
        "Only your active, unpublished local sets or official Pending/Graveyard sets can be nominated.",
    )
}
fn conflict() -> ApiError {
    error(
        StatusCode::CONFLICT,
        "rank_request_conflict",
        "This request changed. Refresh before making a decision.",
    )
}
fn closed() -> ApiError {
    error(
        StatusCode::CONFLICT,
        "rank_request_closed",
        "This request has already been closed.",
    )
}
fn reason(value: &str) -> ApiResult<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.len() > 4000
        || trimmed.chars().count() > 1000
        || trimmed.chars().any(char::is_control)
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "invalid_rank_request_reason",
            "Supply a reason between 1 and 1000 characters without control characters.",
        ));
    }
    Ok(trimmed)
}
fn valid_set(id: i64) -> ApiResult<i64> {
    if !(1..=MAX_SET_ID).contains(&id) {
        Err(ApiError::bad("Supply a valid beatmap set ID."))
    } else {
        Ok(id)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Create {
    native_set_id: Option<i64>,
    official_set_id: Option<i64>,
    difficulty_ids: Option<Vec<i64>>,
    reason: String,
}
impl Create {
    fn target(&self) -> ApiResult<(i64, &'static str)> {
        match (self.native_set_id, self.official_set_id) {
            (Some(id), None) => Ok((
                valid_set(id)?,
                if id >= LOCAL_SET_MIN {
                    "local"
                } else {
                    "official"
                },
            )),
            (None, Some(id)) if (1..LOCAL_SET_MIN).contains(&id) => Ok((id, "official")),
            _ => Err(ApiError::bad(
                "Supply exactly one native set ID or official set ID.",
            )),
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct List {
    page: Option<u32>,
    page_size: Option<u32>,
    status: Option<String>,
    native_set_id: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Withdraw {
    expected_revision: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Review {
    decision: String,
    expected_revision: i64,
    reason: String,
    difficulty_ids: Option<Vec<i64>>,
    difficulty_revisions: Option<Vec<DifficultyRevision>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DifficultyRevision {
    native_id: i64,
    expected_revision: i64,
    #[serde(deserialize_with = "nullable_revision")]
    expected_status_revision: Option<i64>,
}
impl DifficultyRevision {
    pub(crate) fn new(
        native_id: i64,
        expected_revision: i64,
        expected_status_revision: Option<i64>,
    ) -> Self {
        Self {
            native_id,
            expected_revision,
            expected_status_revision,
        }
    }
}
fn nullable_revision<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<i64>, D::Error> {
    Option::<i64>::deserialize(deserializer)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DifficultyPatch {
    status: String,
    expected_revision: i64,
    #[serde(deserialize_with = "nullable_revision")]
    expected_status_revision: Option<i64>,
    reason: String,
}

pub(crate) fn routes() -> axum::Router<AppState> {
    use axum::routing::{get as route_get, patch, post};
    axum::Router::new()
        .route("/api/v1/me/beatmap-rank-requests", route_get(own))
        .route("/api/v1/beatmap-rank-requests", post(create))
        .route("/api/v1/beatmap-rank-requests/{id}", route_get(get))
        .route(
            "/api/v1/beatmap-rank-requests/{id}/withdraw",
            post(withdraw),
        )
        .route("/api/v1/admin/beatmap-rank-requests", route_get(queue))
        .route("/api/v1/admin/beatmap-rank-requests/{id}", patch(review))
        .route("/api/v1/admin/beatmapsets", route_get(staff_local_sets))
        .route("/api/v1/admin/beatmapsets/{id}", route_get(staff_set))
        .route(
            "/api/v1/admin/beatmapsets/{set_id}/difficulties/{native_id}",
            patch(update_difficulty),
        )
        .layer(axum::extract::DefaultBodyLimit::max(96 * 1024))
}

// Only these fields cross the owner/staff boundary. Stored manifests and audit
// details remain server-side; no user email or credentials enter this DTO.
const DTO: &str = r#"jsonb_build_object(
    'id',r.id,'native_set_id',r.native_set_id,'source',r.source,
    'requester',jsonb_build_object('id',u.id,'username',u.username,'country',u.country,'avatar_url',u.avatar_url),
    'reason',r.reason,'status',r.status,'revision',r.revision,
    'metadata',jsonb_build_object('title',r.metadata->'title','artist',r.metadata->'artist',
        'creator',r.metadata->'creator','difficulty_count',r.metadata->'difficulty_count',
        'official_url',r.metadata->'official_url'),
    'review_reason',r.review_reason,'approved_difficulty_ids',r.approved_difficulty_ids,
    'difficulties',(SELECT COALESCE(jsonb_agg(jsonb_build_object(
        'native_id',(m->>'native_id')::bigint,'version',m->'version','mode',m->'mode','stars',m->'stars',
        'status',COALESCE(o.status,b.status,m->>'status'),'revision',COALESCE(o.revision,0),'status_revision',b.status_revision)
        ORDER BY (m->>'native_id')::bigint),'[]'::jsonb)
        FROM jsonb_array_elements(r.metadata->'manifest') m
        LEFT JOIN beatmaps b ON b.native_id=(m->>'native_id')::bigint AND b.native_set_id=r.native_set_id
        LEFT JOIN beatmap_status_overrides o ON o.native_id=(m->>'native_id')::bigint AND o.native_set_id=r.native_set_id),
    'reviewer',CASE WHEN reviewer.id IS NULL THEN NULL ELSE jsonb_build_object('id',reviewer.id,
        'username',reviewer.username,'country',reviewer.country,'avatar_url',reviewer.avatar_url) END,
    'created_at',r.created_at,'updated_at',r.updated_at,'reviewed_at',r.reviewed_at,
    'representative_id',(SELECT min(b.id) FROM active_beatmaps b WHERE b.native_set_id=r.native_set_id))"#;
const JOINS: &str = "FROM beatmap_rank_requests r JOIN users u ON u.id=r.requester_id LEFT JOIN users reviewer ON reviewer.id=r.reviewed_by";

async fn projection<'e, E>(executor: E, id: i64) -> ApiResult<Value>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_scalar(&format!("SELECT {DTO} {JOINS} WHERE r.id=$1"))
        .bind(id)
        .fetch_optional(executor)
        .await?
        .ok_or_else(ApiError::missing)
}

async fn listing(state: &AppState, user: &auth::User, filter: List, staff: bool) -> ApiResult {
    let requested = filter
        .status
        .as_deref()
        .unwrap_or(if staff { "pending" } else { "all" });
    if !matches!(
        requested,
        "all" | "pending" | "approved" | "rejected" | "withdrawn"
    ) {
        return Err(ApiError::bad("Unknown rank request status."));
    }
    let status = (requested != "all").then_some(requested);
    let set_id = filter.native_set_id.map(valid_set).transpose()?;
    let (current, size, offset) = Pagination {
        page: filter.page,
        page_size: filter.page_size,
    }
    .bounds();
    // A single statement provides total and page items from one snapshot.
    let sql = format!(
        r#"WITH matching AS MATERIALIZED (
        SELECT r.* FROM beatmap_rank_requests r WHERE ($1::boolean OR r.requester_id=$2)
            AND ($3::text IS NULL OR r.status=$3) AND ($4::bigint IS NULL OR r.native_set_id=$4)
    ), selected AS (SELECT * FROM matching ORDER BY id DESC LIMIT $5 OFFSET $6)
    SELECT COALESCE((SELECT jsonb_agg(item ORDER BY id DESC) FROM (
        SELECT r.id,{DTO} AS item FROM selected r JOIN users u ON u.id=r.requester_id
        LEFT JOIN users reviewer ON reviewer.id=r.reviewed_by) x),'[]'::jsonb) AS items,
        (SELECT count(*) FROM matching) AS total"#
    );
    let row = sqlx::query(&sql)
        .bind(staff)
        .bind(user.id)
        .bind(status)
        .bind(set_id)
        .bind(size)
        .bind(offset)
        .fetch_one(&state.db)
        .await?;
    let items: Value = row.try_get("items")?;
    Ok(page(
        items
            .as_array()
            .cloned()
            .ok_or_else(|| ApiError::internal("invalid rank request projection"))?,
        row.try_get("total")?,
        current,
        size,
    ))
}

pub(crate) async fn own(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(filter): Query<List>,
) -> ApiResult {
    let user = auth::require(&state, &headers).await?;
    listing(&state, &user, filter, false).await
}
pub(crate) async fn queue(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(filter): Query<List>,
) -> ApiResult {
    let user = auth::require(&state, &headers).await?;
    user.require("beatmaps.rank.review")?;
    listing(&state, &user, filter, true).await
}
pub(crate) async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult {
    let user = auth::require(&state, &headers).await?;
    let owner: i64 =
        sqlx::query_scalar("SELECT requester_id FROM beatmap_rank_requests WHERE id=$1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(ApiError::missing)?;
    if owner != user.id && !has_permission(&user.permissions, "beatmaps.rank.review") {
        return Err(ApiError::missing());
    }
    Ok(Json(projection(&state.db, id).await?))
}

async fn official_snapshot(state: &AppState, id: i64) -> ApiResult<Value> {
    // Reuse the same bounded, cached and globally rate-spaced official detail
    // service as website discovery. Nomination does not fetch a fabricated OSZ.
    let Json(value) = maps::official_detail(State(state.clone()), Path(id)).await?;
    official_snapshot_metadata(value, id)
}

fn official_snapshot_metadata(value: Value, id: i64) -> ApiResult<Value> {
    if value["official_set_id"].as_i64() != Some(id) {
        return Err(ineligible());
    }
    let difficulties = value["difficulties"]
        .as_array()
        .filter(|items| !items.is_empty())
        .ok_or_else(ineligible)?;
    let mut manifest = Vec::with_capacity(difficulties.len());
    for map in difficulties {
        let official_id = map["official_id"]
            .as_i64()
            .filter(|id| (1..LOCAL_SET_MIN).contains(id))
            .ok_or_else(ineligible)?;
        let checksum = map["checksum"]
            .as_str()
            .filter(|s| s.len() == 32 && s.bytes().all(|c| c.is_ascii_hexdigit()))
            .ok_or_else(ineligible)?;
        manifest.push(
            json!({"native_id":official_id,"md5":checksum.to_ascii_lowercase(),
            "version":map["version"],"mode":map["mode"],"stars":map["stars"],
            "status":map.get("status").unwrap_or(&value["status"])}),
        );
    }
    manifest.sort_unstable_by_key(|entry| entry["native_id"].as_i64().unwrap_or_default());
    if manifest
        .windows(2)
        .any(|maps| maps[0]["native_id"] == maps[1]["native_id"])
    {
        return Err(ineligible());
    }
    Ok(
        json!({"title":value["title"],"artist":value["artist"],"creator":value["creator"],
        "difficulty_count":manifest.len(),"official_url":value["official_url"],"upstream_status":value["status"],"manifest":manifest}),
    )
}

async fn official_metadata(state: &AppState, id: i64) -> ApiResult<Value> {
    let snapshot = official_snapshot(state, id).await?;
    if !matches!(
        snapshot["upstream_status"].as_str(),
        Some("pending" | "graveyard")
    ) {
        return Err(ineligible());
    }
    Ok(snapshot)
}

async fn local_metadata(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: i64,
    owner: i64,
) -> ApiResult<Value> {
    let members = sqlx::query("SELECT id,native_id,md5,title,artist,creator,source,uploaded_by,status,version,mode,stars,official_id,official_set_id FROM beatmaps WHERE native_set_id=$1 ORDER BY id FOR UPDATE")
        .bind(id).fetch_all(&mut **tx).await?;
    let first = members.first().ok_or_else(ApiError::missing)?;
    if members.iter().any(|map| {
        map.get::<String, _>("source") != "local"
            || map.get::<Option<i64>, _>("official_id").is_some()
            || map.get::<Option<i64>, _>("official_set_id").is_some()
            || map.get::<Option<i64>, _>("uploaded_by") != Some(owner)
    }) {
        return Err(ineligible());
    }
    let manifest = members.iter().map(|map| json!({"id":map.get::<i64,_>("id"),"native_id":map.get::<i64,_>("native_id"),
        "md5":map.get::<String,_>("md5"),"version":map.get::<String,_>("version"),"mode":map.get::<i16,_>("mode"),
        "stars":map.get::<f64,_>("stars"),"status":map.get::<String,_>("status")})).collect::<Vec<_>>();
    Ok(
        json!({"title":first.get::<String,_>("title"),"artist":first.get::<String,_>("artist"),
        "creator":first.get::<String,_>("creator"),"difficulty_count":manifest.len(),"official_url":null,"manifest":manifest}),
    )
}

fn selected_manifest(metadata: &Value, ids: Option<&[i64]>) -> ApiResult<Value> {
    let all = metadata["manifest"].as_array().ok_or_else(ineligible)?;
    let eligible = |map: &Value| {
        matches!(
            map["status"].as_str(),
            Some("pending" | "graveyard" | "unranked")
        )
    };
    let selected = if let Some(ids) = ids {
        let ids = validated_ids(ids)?;
        let selected = all
            .iter()
            .filter(|map| ids.contains(&map["native_id"].as_i64().unwrap_or_default()))
            .cloned()
            .collect::<Vec<_>>();
        if selected.len() != ids.len() || selected.iter().any(|map| !eligible(map)) {
            return Err(ineligible());
        }
        selected
    } else {
        all.iter()
            .filter(|map| eligible(map))
            .cloned()
            .collect::<Vec<_>>()
    };
    if selected.is_empty() {
        return Err(ineligible());
    }
    let mut result = metadata.clone();
    result["difficulty_count"] = json!(selected.len());
    result["manifest"] = json!(selected);
    Ok(result)
}
fn validated_ids(ids: &[i64]) -> ApiResult<std::collections::BTreeSet<i64>> {
    if !(1..=512).contains(&ids.len()) || ids.iter().any(|id| !(1..=MAX_SET_ID).contains(id)) {
        return Err(ApiError::bad(
            "Choose between 1 and 512 valid difficulty IDs.",
        ));
    }
    let set = ids
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    if set.len() != ids.len() {
        return Err(ApiError::bad("Choose each difficulty only once."));
    }
    Ok(set)
}

pub(crate) async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Create>,
) -> ApiResult {
    let user = auth::require(&state, &headers).await?;
    user.require("beatmaps.rank.request")?;
    let (set_id, source) = input.target()?;
    let reason = reason(&input.reason)?;
    // Reject obvious existing requests before doing any upstream work. The
    // transaction repeats this after set and actor fences for race safety.
    if sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM beatmap_rank_requests WHERE native_set_id=$1 AND status='pending')")
        .bind(set_id).fetch_one(&state.db).await? { return Err(error(StatusCode::CONFLICT,"rank_request_exists","An open request already exists for this set.")); }
    let metadata = if source == "official" {
        let result = official_metadata(&state, set_id).await;
        auth::require(&state, &headers)
            .await?
            .require("beatmaps.rank.request")?;
        Some(result?)
    } else {
        None
    };
    let mut tx = state.db.begin().await?;
    let actor = auth::mutation_actor(&mut tx, &user, &[], auth::MutationScope::Permissions).await?;
    actor.require("beatmaps.rank.request")?;
    availability::lock_exclusive_set(&mut tx, set_id).await?;
    availability::lock_active_set(&mut tx, set_id).await?;
    let metadata = match metadata {
        Some(metadata) => metadata,
        None => local_metadata(&mut tx, set_id, actor.id).await?,
    };
    let metadata = selected_manifest(&metadata, input.difficulty_ids.as_deref())?;
    for map in metadata["manifest"].as_array().ok_or_else(ineligible)? {
        let native_id = map["native_id"].as_i64().ok_or_else(ineligible)?;
        let current = locked_difficulty(&mut tx, set_id, native_id).await?;
        if current.as_ref().is_some_and(|row| {
            matches!(row.get::<String, _>("status").as_str(), "ranked" | "loved")
        }) {
            return Err(ineligible());
        }
        let override_status: Option<String> = sqlx::query_scalar(
            "SELECT status FROM beatmap_status_overrides WHERE native_id=$1 AND native_set_id=$2",
        )
        .bind(native_id)
        .bind(set_id)
        .fetch_optional(&mut *tx)
        .await?;
        if override_status
            .as_deref()
            .is_some_and(|s| matches!(s, "ranked" | "loved"))
        {
            return Err(ineligible());
        }
    }
    let counts = sqlx::query("SELECT count(*) FILTER(WHERE status='pending') AS open,count(*) FILTER(WHERE created_at>clock_timestamp()-interval '24 hours') AS daily FROM beatmap_rank_requests WHERE requester_id=$1")
        .bind(actor.id).fetch_one(&mut *tx).await?;
    if counts.get::<i64, _>("open") >= MAX_OPEN_REQUESTS
        || counts.get::<i64, _>("daily") >= MAX_DAILY_REQUESTS
    {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "rank_request_limit",
            "Nomination limit reached. Close an open request or try again later.",
        ));
    }
    if sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM beatmap_rank_requests WHERE native_set_id=$1 AND status='pending')")
        .bind(set_id).fetch_one(&mut *tx).await? { return Err(error(StatusCode::CONFLICT,"rank_request_exists","An open request already exists for this set.")); }
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    let id:i64 = sqlx::query_scalar("INSERT INTO beatmap_rank_requests(native_set_id,source,requester_id,reason,metadata) VALUES($1,$2,$3,$4,$5) RETURNING id")
        .bind(set_id).bind(source).bind(actor.id).bind(reason).bind(metadata).fetch_one(&mut *tx).await?;
    sqlx::query("INSERT INTO audit_events(actor_id,action,target_type,target_id,details) VALUES($1,'beatmaps.rank_request.create','beatmap_rank_request',$2,$3)")
        .bind(actor.id).bind(id.to_string()).bind(json!({"native_set_id":set_id,"source":source})).execute(&mut *tx).await?;
    let value = projection(&mut *tx, id).await?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    tx.commit().await?;
    Ok(Json(value))
}

async fn locked_request(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: i64,
    revision: i64,
) -> ApiResult<sqlx::postgres::PgRow> {
    if revision < 0 {
        return Err(ApiError::bad("Supply a non-negative request revision."));
    }
    let row = sqlx::query("SELECT * FROM beatmap_rank_requests WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(ApiError::missing)?;
    if row.get::<i64, _>("revision") != revision {
        return Err(conflict());
    }
    if row.get::<String, _>("status") != "pending" {
        return Err(closed());
    }
    Ok(row)
}

pub(crate) async fn withdraw(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<Withdraw>,
) -> ApiResult {
    let user = auth::require(&state, &headers).await?;
    // Ownership is the sole authority for withdrawing one's existing request;
    // losing the create grant does not trap an open nomination.
    let mut tx = state.db.begin().await?;
    let actor = auth::mutation_actor(&mut tx, &user, &[], auth::MutationScope::Session).await?;
    let owner: i64 =
        sqlx::query_scalar("SELECT requester_id FROM beatmap_rank_requests WHERE id=$1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(ApiError::missing)?;
    if owner != actor.id {
        return Err(ApiError::missing());
    }
    let row = locked_request(&mut tx, id, input.expected_revision).await?;
    let set_id: i64 = row.get("native_set_id");
    sqlx::query("UPDATE beatmap_rank_requests SET status='withdrawn' WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO audit_events(actor_id,action,target_type,target_id,details) VALUES($1,'beatmaps.rank_request.withdraw','beatmap_rank_request',$2,$3)")
        .bind(actor.id).bind(id.to_string()).bind(json!({"native_set_id":set_id,"previous_revision":input.expected_revision})).execute(&mut *tx).await?;
    let value = projection(&mut *tx, id).await?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    tx.commit().await?;
    Ok(Json(value))
}

fn valid_status(status: &str) -> ApiResult<()> {
    if !matches!(
        status,
        "pending" | "wip" | "graveyard" | "unranked" | "ranked" | "loved"
    ) {
        return Err(ApiError::bad("Unknown beatmap status."));
    }
    Ok(())
}
fn status_conflict() -> ApiError {
    error(
        StatusCode::CONFLICT,
        "beatmap_status_conflict",
        "This difficulty changed. Refresh before saving its status.",
    )
}
pub(crate) async fn locked_difficulty(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    set_id: i64,
    native_id: i64,
) -> ApiResult<Option<sqlx::postgres::PgRow>> {
    Ok(sqlx::query("SELECT id,native_id,native_set_id,md5,status,status_revision,source,uploaded_by FROM beatmaps WHERE native_id=$1 AND native_set_id=$2 FOR UPDATE")
        .bind(native_id).bind(set_id).fetch_optional(&mut **tx).await?)
}
pub(crate) async fn check_difficulty_revision(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    set_id: i64,
    revision: &DifficultyRevision,
    row: Option<&sqlx::postgres::PgRow>,
) -> ApiResult<()> {
    if revision.expected_revision < 0 || revision.expected_status_revision.is_some_and(|r| r < 0) {
        return Err(ApiError::bad("Supply non-negative difficulty revisions."));
    }
    let actual:Option<i64>=sqlx::query_scalar("SELECT revision FROM beatmap_status_overrides WHERE native_id=$1 AND native_set_id=$2 FOR UPDATE")
        .bind(revision.native_id).bind(set_id).fetch_optional(&mut **tx).await?;
    if actual.unwrap_or(0) != revision.expected_revision
        || row.map(|r| r.get::<i64, _>("status_revision")) != revision.expected_status_revision
    {
        return Err(status_conflict());
    }
    Ok(())
}

/// Caller holds fresh authority and the exclusive set fence, then acquires the
/// difficulty row before its override. Only this native identity is changed.
/// Recording the override first lets later official imports/refreshes inherit
/// the decision without changing siblings or inventing a package download.
pub(crate) async fn set_difficulty_status(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: &auth::MutationActor,
    set_id: i64,
    native_id: i64,
    status: &str,
    reason: &str,
    row: Option<&sqlx::postgres::PgRow>,
) -> ApiResult<()> {
    actor.require("beatmaps.moderate")?;
    valid_status(status)?;
    let before = json!({"status":row.map(|r|r.get::<String,_>("status")),
        "status_revision":row.map(|r|r.get::<i64,_>("status_revision"))});
    let result=sqlx::query("INSERT INTO beatmap_status_overrides(native_id,native_set_id,status,actor_id,reason) VALUES($1,$2,$3,$4,$5) ON CONFLICT(native_id) DO UPDATE SET status=EXCLUDED.status,actor_id=EXCLUDED.actor_id,reason=EXCLUDED.reason,changed_at=clock_timestamp() WHERE beatmap_status_overrides.native_set_id=EXCLUDED.native_set_id")
        .bind(native_id).bind(set_id).bind(status).bind(actor.id).bind(reason).execute(&mut **tx).await?;
    if result.rows_affected() != 1 {
        return Err(status_conflict());
    }
    if let Some(row) = row {
        // Preserve the existing moderation wait/expiry path for legacy callers.
        sqlx::query("UPDATE beatmaps SET status=$2 WHERE id=$1")
            .bind(row.get::<i64, _>("id"))
            .bind(status)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query("INSERT INTO audit_events(actor_id,action,target_type,target_id,details) VALUES($1,'beatmaps.status','beatmap',$2,$3)")
        .bind(actor.id).bind(row.map(|r|r.get::<i64,_>("id").to_string()).unwrap_or_else(||native_id.to_string()))
        .bind(json!({"native_id":native_id,"native_set_id":set_id,"status":status,"reason":reason,"previous":before}))
        .execute(&mut **tx).await?;
    Ok(())
}

async fn staff_snapshot(state: &AppState, headers: &HeaderMap, set_id: i64) -> ApiResult<Value> {
    if set_id < LOCAL_SET_MIN {
        return official_snapshot_metadata(
            maps::official_staff_detail(state, headers, set_id).await?,
            set_id,
        );
    }
    let rows=sqlx::query("SELECT id,native_id,md5,title,artist,creator,version,mode,stars,status,source,uploaded_by,official_id,official_set_id FROM beatmaps WHERE native_set_id=$1 ORDER BY native_id")
        .bind(set_id).fetch_all(&state.db).await?;
    let first = rows.first().ok_or_else(ApiError::missing)?;
    // The staff catalogue is for genuine community uploads, not historical
    // local-labelled official imports or orphan fixtures.
    if rows.iter().any(|r| {
        r.get::<String, _>("source") != "local"
            || r.get::<Option<i64>, _>("uploaded_by").is_none()
            || r.get::<Option<i64>, _>("official_id").is_some()
            || r.get::<Option<i64>, _>("official_set_id").is_some()
    }) {
        return Err(ApiError::missing());
    }
    let manifest=rows.iter().map(|r|json!({"id":r.get::<i64,_>("id"),"native_id":r.get::<i64,_>("native_id"),
        "md5":r.get::<String,_>("md5"),"version":r.get::<String,_>("version"),"mode":r.get::<i16,_>("mode"),
        "stars":r.get::<f64,_>("stars"),"status":r.get::<String,_>("status")})).collect::<Vec<_>>();
    Ok(
        json!({"title":first.get::<String,_>("title"),"artist":first.get::<String,_>("artist"),
        "creator":first.get::<String,_>("creator"),"difficulty_count":manifest.len(),"official_url":null,"manifest":manifest}),
    )
}
async fn set_projection(
    db: &mut sqlx::PgConnection,
    set_id: i64,
    metadata: &Value,
) -> ApiResult<Value> {
    let rows:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('native_id',(m->>'native_id')::bigint,'version',m->'version','mode',m->'mode','stars',m->'stars','status',COALESCE(o.status,b.status,m->>'status'),'revision',COALESCE(o.revision,0),'status_revision',b.status_revision) FROM jsonb_array_elements($1::jsonb) m LEFT JOIN beatmaps b ON b.native_id=(m->>'native_id')::bigint AND b.native_set_id=$2 LEFT JOIN beatmap_status_overrides o ON o.native_id=(m->>'native_id')::bigint AND o.native_set_id=$2 ORDER BY (m->>'native_id')::bigint")
        .bind(&metadata["manifest"]).bind(set_id).fetch_all(&mut *db).await?;
    let representative: Option<i64> =
        sqlx::query_scalar("SELECT min(id) FROM beatmaps WHERE native_set_id=$1")
            .bind(set_id)
            .fetch_one(&mut *db)
            .await?;
    let availability:Value=sqlx::query_scalar("SELECT COALESCE((SELECT jsonb_build_object('withdrawn',withdrawn,'revision',revision,'reason',reason,'changed_at',changed_at) FROM beatmapset_availability WHERE native_set_id=$1),jsonb_build_object('withdrawn',false,'revision',0,'reason',NULL,'changed_at',NULL))")
        .bind(set_id).fetch_one(&mut *db).await?;
    Ok(
        json!({"native_set_id":set_id,"source":if set_id<LOCAL_SET_MIN{"official"}else{"local"},
        "title":metadata["title"],"artist":metadata["artist"],"creator":metadata["creator"],
        "difficulties":rows,"representative_id":representative,"availability":availability}),
    )
}
fn verify_catalogue_decision(
    snapshot: &Value,
    manifest: &[Value],
    selected: &[i64],
) -> ApiResult<()> {
    let maps = snapshot["beatmaps"]
        .as_array()
        .ok_or_else(status_conflict)?;
    for native_id in selected {
        let proof = manifest
            .iter()
            .find(|m| m["native_id"].as_i64() == Some(*native_id))
            .ok_or_else(status_conflict)?;
        let current = maps
            .iter()
            .find(|m| m["id"].as_i64() == Some(*native_id))
            .ok_or_else(status_conflict)?;
        if proof["md5"]
            .as_str()
            .zip(current["checksum"].as_str())
            .is_none_or(|(a, b)| !a.eq_ignore_ascii_case(b))
            || proof["mode"] != current["mode_int"]
        {
            return Err(status_conflict());
        }
    }
    Ok(())
}
pub(crate) async fn staff_set(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(set_id): Path<i64>,
) -> ApiResult {
    let user = auth::require(&state, &headers).await?;
    user.require("beatmaps.moderate")?;
    let set_id = valid_set(set_id)?;
    let result = staff_snapshot(&state, &headers, set_id).await;
    // Permission loss during upstream work must take precedence over its result.
    auth::require(&state, &headers)
        .await?
        .require("beatmaps.moderate")?;
    let metadata = result?;
    let mut tx = state.db.begin().await?;
    let actor = auth::mutation_actor(&mut tx, &user, &[], auth::MutationScope::Permissions).await?;
    actor.require("beatmaps.moderate")?;
    let value = set_projection(&mut tx, set_id, &metadata).await?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    tx.commit().await?;
    Ok(Json(value))
}

pub(crate) async fn update_difficulty(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((set_id, native_id)): Path<(i64, i64)>,
    Json(input): Json<DifficultyPatch>,
) -> ApiResult {
    let user = auth::require(&state, &headers).await?;
    user.require("beatmaps.moderate")?;
    let set_id = valid_set(set_id)?;
    valid_set(native_id)?;
    valid_status(&input.status)?;
    let reason = reason(&input.reason)?;
    let result = staff_snapshot(&state, &headers, set_id).await;
    let catalogue = if set_id < LOCAL_SET_MIN && result.is_ok() {
        Some(maps::catalogue_snapshot(&state, set_id).await)
    } else {
        None
    };
    auth::require(&state, &headers)
        .await?
        .require("beatmaps.moderate")?;
    let metadata = result?;
    let catalogue = catalogue.transpose()?;
    let manifest = metadata["manifest"]
        .as_array()
        .ok_or_else(ApiError::missing)?;
    let member = manifest
        .iter()
        .find(|map| map["native_id"].as_i64() == Some(native_id))
        .ok_or_else(ApiError::missing)?;
    if let Some(snapshot) = &catalogue {
        verify_catalogue_decision(snapshot, manifest, &[native_id])?;
    }
    let mut tx = state.db.begin().await?;
    let actor = auth::mutation_actor(&mut tx, &user, &[], auth::MutationScope::Permissions).await?;
    actor.require("beatmaps.moderate")?;
    availability::lock_exclusive_set(&mut tx, set_id).await?;
    availability::lock_active_set(&mut tx, set_id).await?;
    let row = locked_difficulty(&mut tx, set_id, native_id).await?;
    if row
        .as_ref()
        .is_some_and(|r| member["md5"].as_str() != Some(r.get::<String, _>("md5").as_str()))
    {
        return Err(status_conflict());
    }
    if set_id >= LOCAL_SET_MIN && row.is_none() {
        return Err(ApiError::missing());
    }
    check_difficulty_revision(
        &mut tx,
        set_id,
        &DifficultyRevision {
            native_id,
            expected_revision: input.expected_revision,
            expected_status_revision: input.expected_status_revision,
        },
        row.as_ref(),
    )
    .await?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    set_difficulty_status(
        &mut tx,
        &actor,
        set_id,
        native_id,
        &input.status,
        reason,
        row.as_ref(),
    )
    .await?;
    if let Some(snapshot) = catalogue {
        server_catalogue::publish(&mut tx, set_id, &snapshot, &[native_id]).await?;
    }
    let value = set_projection(&mut tx, set_id, &metadata).await?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    tx.commit().await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
pub(crate) struct LocalList {
    page: Option<u32>,
    page_size: Option<u32>,
    q: Option<String>,
    availability: Option<String>,
}
pub(crate) async fn staff_local_sets(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(filter): Query<LocalList>,
) -> ApiResult {
    let user = auth::require(&state, &headers).await?;
    user.require("beatmaps.moderate")?;
    let available = filter.availability.as_deref().unwrap_or("all");
    if !matches!(available, "all" | "active" | "withdrawn") {
        return Err(ApiError::bad("Unknown availability filter."));
    }
    let q = filter.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    if q.is_some_and(|q| q.chars().count() > 100 || q.chars().any(char::is_control)) {
        return Err(ApiError::bad(
            "Search must be at most 100 characters without control characters.",
        ));
    }
    let q = q.map(|q| format!("%{q}%"));
    let (current, size, offset) = Pagination {
        page: filter.page,
        page_size: filter.page_size,
    }
    .bounds();
    let row=sqlx::query(r#"WITH matching AS MATERIALIZED (
        SELECT b.* FROM beatmaps b WHERE source='local' AND official_id IS NULL AND official_set_id IS NULL
            AND uploaded_by IS NOT NULL AND native_set_id>=2000000000
            AND ($1::text IS NULL OR EXISTS(SELECT 1 FROM beatmaps matched WHERE matched.native_set_id=b.native_set_id
                AND (matched.title ILIKE $1 OR matched.artist ILIKE $1 OR matched.creator ILIKE $1 OR matched.version ILIKE $1)))
            AND ($2='all' OR ($2='active' AND illya_beatmapset_active(native_set_id))
                OR ($2='withdrawn' AND NOT illya_beatmapset_active(native_set_id)))
    ), grouped AS (SELECT native_set_id,min(id) representative_id,count(*) difficulty_count,
        array_agg(DISTINCT status ORDER BY status) statuses,max(created_at) added_at FROM matching GROUP BY native_set_id),
    selected AS (SELECT * FROM grouped ORDER BY added_at DESC,native_set_id DESC LIMIT $3 OFFSET $4)
    SELECT COALESCE((SELECT jsonb_agg(jsonb_build_object('native_set_id',g.native_set_id,
        'representative_id',g.representative_id,'title',b.title,'artist',b.artist,'creator',b.creator,
        'difficulty_count',g.difficulty_count,'statuses',g.statuses,'added_at',g.added_at,
        'withdrawn',NOT illya_beatmapset_active(g.native_set_id),
        'uploaded_by',jsonb_build_object('id',u.id,'username',u.username,'country',u.country,'avatar_url',u.avatar_url))
        ORDER BY g.added_at DESC,g.native_set_id DESC) FROM selected g JOIN matching b ON b.id=g.representative_id
        JOIN users u ON u.id=b.uploaded_by),'[]'::jsonb) items,(SELECT count(*) FROM grouped) total"#)
        .bind(q).bind(available).bind(size).bind(offset).fetch_one(&state.db).await?;
    let items: Value = row.try_get("items")?;
    Ok(page(
        items
            .as_array()
            .cloned()
            .ok_or_else(|| ApiError::internal("invalid local set projection"))?,
        row.try_get("total")?,
        current,
        size,
    ))
}

pub(crate) async fn review(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<Review>,
) -> ApiResult {
    let user = auth::require(&state, &headers).await?;
    user.require("beatmaps.rank.review")?;
    if !matches!(input.decision.as_str(), "approved" | "rejected") {
        return Err(ApiError::bad("Choose approval or rejection."));
    }
    if input.expected_revision < 0 {
        return Err(ApiError::bad("Supply a non-negative request revision."));
    }
    let reason = reason(&input.reason)?;
    let selected = if input.decision == "approved" {
        user.require("beatmaps.moderate")?;
        let ids = validated_ids(
            input
                .difficulty_ids
                .as_deref()
                .ok_or_else(|| ApiError::bad("Select the difficulties to rank."))?,
        )?;
        let revisions = input
            .difficulty_revisions
            .as_ref()
            .filter(|r| r.len() == ids.len())
            .ok_or_else(|| {
                ApiError::bad("Supply current revisions for each selected difficulty.")
            })?;
        let revision_ids = revisions.iter().map(|r| r.native_id).collect::<Vec<_>>();
        if validated_ids(&revision_ids)? != ids {
            return Err(ApiError::bad(
                "Supply exactly one revision for each selected difficulty.",
            ));
        }
        Some(ids)
    } else {
        if input.difficulty_ids.is_some() || input.difficulty_revisions.is_some() {
            return Err(ApiError::bad(
                "Difficulty selections are only accepted for approval.",
            ));
        }
        None
    };
    // Verify the private request before upstream work, then repeat every check
    // against the locked row. Request/target identity is immutable in PostgreSQL.
    let preview=sqlx::query("SELECT native_set_id,source,revision,status,requester_id FROM beatmap_rank_requests WHERE id=$1")
        .bind(id).fetch_optional(&state.db).await?.ok_or_else(ApiError::missing)?;
    if preview.get::<i64, _>("revision") != input.expected_revision {
        return Err(conflict());
    }
    if preview.get::<String, _>("status") != "pending" {
        return Err(closed());
    }
    let set_id: i64 = preview.get("native_set_id");
    let official =
        if input.decision == "approved" && preview.get::<String, _>("source") == "official" {
            let result = official_metadata(&state, set_id).await;
            let current = auth::require(&state, &headers).await?;
            current.require("beatmaps.rank.review")?;
            current.require("beatmaps.moderate")?;
            Some(result?)
        } else {
            None
        };
    let catalogue = if official.is_some() {
        let result = maps::catalogue_snapshot(&state, set_id).await;
        let current = auth::require(&state, &headers).await?;
        current.require("beatmaps.rank.review")?;
        current.require("beatmaps.moderate")?;
        Some(result?)
    } else {
        None
    };
    let mut tx = state.db.begin().await?;
    let actor = auth::mutation_actor(&mut tx, &user, &[], auth::MutationScope::Permissions).await?;
    actor.require("beatmaps.rank.review")?;
    if input.decision == "approved" {
        actor.require("beatmaps.moderate")?;
    }
    // The set fence precedes request/map resource rows on both create/review.
    availability::lock_exclusive_set(&mut tx, set_id).await?;
    let row = locked_request(&mut tx, id, input.expected_revision).await?;
    if input.decision == "approved" {
        availability::lock_active_set(&mut tx, set_id).await?;
        let metadata: Value = row.get("metadata");
        let requested = metadata["manifest"].as_array().ok_or_else(ineligible)?;
        let current = match official {
            Some(metadata) => metadata,
            None => local_metadata(&mut tx, set_id, row.get("requester_id")).await?,
        };
        let current = current["manifest"].as_array().ok_or_else(ineligible)?;
        if let Some(snapshot) = &catalogue {
            let ids = selected
                .as_ref()
                .ok_or_else(ineligible)?
                .iter()
                .copied()
                .collect::<Vec<_>>();
            verify_catalogue_decision(snapshot, current, &ids)?;
        }
        for native_id in selected.as_ref().ok_or_else(ineligible)? {
            let requested = requested
                .iter()
                .find(|m| m["native_id"].as_i64() == Some(*native_id))
                .ok_or_else(ineligible)?;
            let current=current.iter().find(|m|m["native_id"].as_i64()==Some(*native_id))
                .filter(|m|m["md5"]==requested["md5"]).ok_or_else(||error(StatusCode::CONFLICT,"rank_request_map_changed","A requested difficulty changed. Withdraw this request and nominate its current version."))?;
            if !matches!(
                current["status"].as_str(),
                Some("pending" | "graveyard" | "unranked")
            ) {
                return Err(ineligible());
            }
            let member = locked_difficulty(&mut tx, set_id, *native_id).await?;
            if member.as_ref().is_some_and(|m| {
                m.get::<String, _>("md5") != requested["md5"].as_str().unwrap_or_default()
                    || matches!(m.get::<String, _>("status").as_str(), "ranked" | "loved")
            }) {
                return Err(ineligible());
            }
            let revision = input
                .difficulty_revisions
                .as_ref()
                .and_then(|items| items.iter().find(|r| r.native_id == *native_id))
                .ok_or_else(ineligible)?;
            check_difficulty_revision(&mut tx, set_id, revision, member.as_ref()).await?;
            let override_status:Option<String>=sqlx::query_scalar("SELECT status FROM beatmap_status_overrides WHERE native_id=$1 AND native_set_id=$2")
                .bind(native_id).bind(set_id).fetch_optional(&mut *tx).await?;
            if override_status
                .as_deref()
                .is_some_and(|s| matches!(s, "ranked" | "loved"))
            {
                return Err(ineligible());
            }
            auth::ensure_mutation_actor(&mut tx, &actor).await?;
            set_difficulty_status(
                &mut tx,
                &actor,
                set_id,
                *native_id,
                "ranked",
                reason,
                member.as_ref(),
            )
            .await?;
        }
    }
    if let Some(snapshot) = catalogue {
        let reauthorized = selected
            .as_ref()
            .map(|ids| ids.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        server_catalogue::publish(&mut tx, set_id, &snapshot, &reauthorized).await?;
    }
    let approved = selected
        .as_ref()
        .map(|ids| ids.iter().copied().collect::<Vec<_>>());
    sqlx::query("UPDATE beatmap_rank_requests SET status=$2,review_reason=$3,reviewed_by=$4,reviewed_at=clock_timestamp(),approved_difficulty_ids=$5 WHERE id=$1")
        .bind(id).bind(&input.decision).bind(reason).bind(actor.id).bind(&approved).execute(&mut *tx).await?;
    crate::notifications::rank_request_decided(&mut tx, id).await?;
    sqlx::query("INSERT INTO audit_events(actor_id,action,target_type,target_id,details) VALUES($1,'beatmaps.rank_request.review','beatmap_rank_request',$2,$3)")
        .bind(actor.id).bind(id.to_string()).bind(json!({"native_set_id":set_id,"decision":input.decision,"difficulty_ids":approved,"reason":reason,"previous_revision":input.expected_revision})).execute(&mut *tx).await?;
    let value = projection(&mut *tx, id).await?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    tx.commit().await?;
    Ok(Json(value))
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    #[test]
    fn decision_cannot_publish_a_different_checksum_or_ruleset_snapshot() {
        let manifest = vec![json!({"native_id":31,"md5":"a".repeat(32),"mode":0})];
        let matching = json!({"beatmaps":[{"id":31,"checksum":"a".repeat(32),"mode_int":0}]});
        assert!(verify_catalogue_decision(&matching, &manifest, &[31]).is_ok());
        for changed in [
            json!({"beatmaps":[{"id":31,"checksum":"b".repeat(32),"mode_int":0}]}),
            json!({"beatmaps":[{"id":31,"checksum":"a".repeat(32),"mode_int":3}]}),
            json!({"beatmaps":[{"id":32,"checksum":"a".repeat(32),"mode_int":0}]}),
        ] {
            assert!(verify_catalogue_decision(&changed, &manifest, &[31]).is_err());
        }
    }
    #[test]
    fn reasons_and_target_selectors_are_bounded() {
        assert_eq!(reason("  ready for review  ").unwrap(), "ready for review");
        for invalid in ["", "   ", "line\nline", "nul\0byte", "\u{7f}"] {
            assert!(reason(invalid).is_err());
        }
        assert!(reason(&"譜".repeat(1000)).is_ok());
        assert!(reason(&"譜".repeat(1001)).is_err());
        for (native, official) in [
            (None, None),
            (Some(1), Some(1)),
            (Some(0), None),
            (Some(MAX_SET_ID + 1), None),
            (None, Some(LOCAL_SET_MIN)),
        ] {
            assert!(
                Create {
                    native_set_id: native,
                    official_set_id: official,
                    difficulty_ids: None,
                    reason: "ok".into()
                }
                .target()
                .is_err()
            );
        }
        assert_eq!(
            Create {
                native_set_id: Some(LOCAL_SET_MIN),
                official_set_id: None,
                difficulty_ids: None,
                reason: "ok".into()
            }
            .target()
            .unwrap(),
            (LOCAL_SET_MIN, "local")
        );
        assert_eq!(
            Create {
                native_set_id: None,
                official_set_id: Some(1),
                difficulty_ids: None,
                reason: "ok".into()
            }
            .target()
            .unwrap(),
            (1, "official")
        );
    }
}
