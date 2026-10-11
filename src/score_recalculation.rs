//! Operator-started bulk pp recalculation.
//!
//! An operator describes which scores to recalculate (player, ruleset, variant,
//! date range, beatmap, calculation version, pp range or explicit IDs), previews
//! the match, then starts a job. One job runs at a time in the background:
//! scores are processed in ID order, each in its own transaction that re-checks
//! the stored score before writing, and the job records progress, the scores
//! whose pp changed, and any that were skipped or failed.
use crate::{ApiError, ApiResult, AppState, auth, scores};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use chrono::NaiveDate;
use illya_domain::ScoreVariant;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row, postgres::PgRow};
use std::time::Duration;

const BATCH: i64 = 25;
const MAX_SCORE_IDS: usize = 1000;
const IDLE_POLL: Duration = Duration::from_secs(3);
const ERROR_POLL: Duration = Duration::from_secs(10);
const BATCH_PAUSE: Duration = Duration::from_millis(100);
const PP_EPSILON: f64 = 1e-9;

/// What the operator typed. Names and IDs are resolved before anything runs.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterInput {
    /// Username or numeric player ID.
    user: Option<String>,
    variant: Option<String>,
    mode: Option<i16>,
    /// First day to include (UTC).
    from: Option<NaiveDate>,
    /// Last day to include (UTC).
    to: Option<NaiveDate>,
    /// Beatmap ID as shown on the site.
    beatmap_id: Option<i64>,
    /// `pending`, `verified` or `all` (default).
    verification: Option<String>,
    #[serde(default)]
    outdated_only: bool,
    min_pp: Option<f64>,
    max_pp: Option<f64>,
    score_ids: Option<Vec<i64>>,
}

/// Validated filters with names resolved; this is what a job stores.
#[derive(Clone, Serialize, Deserialize)]
struct Filters {
    user_id: Option<i64>,
    user_name: Option<String>,
    variant: Option<String>,
    mode: Option<i16>,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    beatmap_internal_id: Option<i64>,
    beatmap_id: Option<i64>,
    verification: Vec<String>,
    outdated_only: bool,
    min_pp: Option<f64>,
    max_pp: Option<f64>,
    score_ids: Option<Vec<i64>>,
}

/// Failed scores always have 0 pp and restricted players are hidden, so neither is recalculated.
const MATCH: &str = "FROM scores s JOIN beatmaps b ON b.id=s.beatmap_id JOIN users u ON u.id=s.user_id
 WHERE s.passed AND NOT u.is_restricted AND s.verification=ANY($1)
 AND ($2::bigint IS NULL OR s.user_id=$2)
 AND ($3::text IS NULL OR s.score_variant=$3)
 AND ($4::smallint IS NULL OR s.mode=$4)
 AND ($5::date IS NULL OR s.created_at>=($5::date::timestamp AT TIME ZONE 'UTC'))
 AND ($6::date IS NULL OR s.created_at<(($6::date+1)::timestamp AT TIME ZONE 'UTC'))
 AND ($7::bigint IS NULL OR s.beatmap_id=$7)
 AND (NOT $8 OR s.pp_calculation_version IS DISTINCT FROM CASE WHEN s.score_variant='relax' THEN $9 ELSE $10 END)
 AND ($11::float8 IS NULL OR s.pp>=$11)
 AND ($12::float8 IS NULL OR s.pp<=$12)
 AND ($13::bigint[] IS NULL OR s.id=ANY($13))";

type Query<'q> = sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>;

fn bind_match<'q>(query: Query<'q>, f: &'q Filters) -> Query<'q> {
    query
        .bind(&f.verification)
        .bind(f.user_id)
        .bind(f.variant.as_deref())
        .bind(f.mode)
        .bind(f.from)
        .bind(f.to)
        .bind(f.beatmap_internal_id)
        .bind(f.outdated_only)
        .bind(scores::PP_VERSION_RELAX)
        .bind(scores::PP_VERSION_VANILLA)
        .bind(f.min_pp)
        .bind(f.max_pp)
        .bind(f.score_ids.as_deref())
}

fn finite_pp(value: Option<f64>, label: &str) -> ApiResult<Option<f64>> {
    match value {
        Some(v) if !v.is_finite() || v < 0.0 => Err(ApiError::bad(format!(
            "{label} must be a non-negative number."
        ))),
        other => Ok(other),
    }
}

fn validate(input: &FilterInput) -> ApiResult<()> {
    if input
        .variant
        .as_deref()
        .is_some_and(|v| !matches!(v, "vanilla" | "relax"))
    {
        return Err(ApiError::bad("Variant must be vanilla or relax."));
    }
    if input.mode.is_some_and(|m| !(0..=3).contains(&m)) {
        return Err(ApiError::bad("Unsupported ruleset."));
    }
    if input
        .verification
        .as_deref()
        .is_some_and(|v| !matches!(v, "pending" | "verified" | "all"))
    {
        return Err(ApiError::bad(
            "Verification must be pending, verified or all.",
        ));
    }
    if let (Some(from), Some(to)) = (input.from, input.to)
        && from > to
    {
        return Err(ApiError::bad(
            "The start date must not be after the end date.",
        ));
    }
    if let (Some(min), Some(max)) = (input.min_pp, input.max_pp)
        && min > max
    {
        return Err(ApiError::bad("The minimum pp must not exceed the maximum."));
    }
    if input.beatmap_id.is_some_and(|id| id <= 0) {
        return Err(ApiError::bad("Beatmap ID must be positive."));
    }
    if input.user.as_deref().is_some_and(|u| u.trim().len() > 64) {
        return Err(ApiError::bad("Player is too long."));
    }
    if let Some(ids) = &input.score_ids {
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        if ids.is_empty()
            || ids.len() > MAX_SCORE_IDS
            || sorted.iter().any(|&id| id <= 0)
            || sorted.windows(2).any(|pair| pair[0] == pair[1])
        {
            return Err(ApiError::bad(format!(
                "Choose between one and {MAX_SCORE_IDS} unique positive score IDs."
            )));
        }
    }
    Ok(())
}

async fn resolve(db: &PgPool, input: FilterInput) -> ApiResult<Filters> {
    validate(&input)?;
    let (user_id, user_name) = match input
        .user
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        None => (None, None),
        Some(user) => {
            let row = if user.bytes().all(|b| b.is_ascii_digit()) {
                let id: i64 = user
                    .parse()
                    .map_err(|_| ApiError::bad("Player ID is out of range."))?;
                sqlx::query("SELECT id,username FROM users WHERE id=$1")
                    .bind(id)
                    .fetch_optional(db)
                    .await?
            } else {
                sqlx::query("SELECT id,username FROM users WHERE lower(username)=lower($1)")
                    .bind(user)
                    .fetch_optional(db)
                    .await?
            };
            let row = row.ok_or_else(|| {
                ApiError(
                    StatusCode::NOT_FOUND,
                    "player_not_found",
                    "No player matches that name or ID.".into(),
                )
            })?;
            (Some(row.get("id")), Some(row.get("username")))
        }
    };
    let beatmap_internal_id = match input.beatmap_id {
        None => None,
        Some(native) => Some(
            sqlx::query_scalar::<_, i64>("SELECT id FROM beatmaps WHERE native_id=$1")
                .bind(native)
                .fetch_optional(db)
                .await?
                .ok_or_else(|| {
                    ApiError(
                        StatusCode::NOT_FOUND,
                        "beatmap_not_found",
                        "No beatmap matches that ID.".into(),
                    )
                })?,
        ),
    };
    let verification = match input.verification.as_deref() {
        Some("pending") => vec!["pending".to_owned()],
        Some("verified") => vec!["verified".to_owned()],
        _ => vec!["pending".to_owned(), "verified".to_owned()],
    };
    Ok(Filters {
        user_id,
        user_name,
        variant: input.variant,
        mode: input.mode,
        from: input.from,
        to: input.to,
        beatmap_internal_id,
        beatmap_id: input.beatmap_id,
        verification,
        outdated_only: input.outdated_only,
        min_pp: finite_pp(input.min_pp, "The minimum pp")?,
        max_pp: finite_pp(input.max_pp, "The maximum pp")?,
        score_ids: input.score_ids,
    })
}

async fn require_operator(state: &AppState, headers: &HeaderMap) -> ApiResult<auth::User> {
    let user = auth::require(state, headers).await?;
    user.require("scores.moderate")?;
    Ok(user)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewInput {
    filters: FilterInput,
}

pub async fn preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PreviewInput>,
) -> ApiResult {
    require_operator(&state, &headers).await?;
    let filters = resolve(&state.db, input.filters).await?;
    let counts = bind_match(
        sqlx::query(&format!(
            "SELECT count(*)::bigint total,
                count(*) FILTER (WHERE s.pp_calculation_version IS DISTINCT FROM CASE WHEN s.score_variant='relax' THEN $9 ELSE $10 END)::bigint outdated,
                count(*) FILTER (WHERE s.score_variant='relax')::bigint relax,
                count(*) FILTER (WHERE s.score_variant<>'relax')::bigint vanilla,
                count(*) FILTER (WHERE s.mode=0)::bigint m0,count(*) FILTER (WHERE s.mode=1)::bigint m1,
                count(*) FILTER (WHERE s.mode=2)::bigint m2,count(*) FILTER (WHERE s.mode=3)::bigint m3,
                count(DISTINCT s.user_id)::bigint players,
                COALESCE(sum(s.pp),0)::float8 pp_sum {MATCH}"
        )),
        &filters,
    )
    .fetch_one(&state.db)
    .await?;
    let sample = bind_match(
        sqlx::query(&format!(
            "SELECT s.id,s.user_id,u.username,b.title,b.artist,b.version,b.native_id,s.mode,s.score_variant,s.pp,s.pp_calculation_version,s.created_at {MATCH} ORDER BY s.created_at DESC,s.id DESC LIMIT 10"
        )),
        &filters,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({
        "filters": filters,
        "total": counts.get::<i64,_>("total"),
        "outdated": counts.get::<i64,_>("outdated"),
        "players": counts.get::<i64,_>("players"),
        "pp_sum": counts.get::<f64,_>("pp_sum"),
        "by_variant": {"vanilla":counts.get::<i64,_>("vanilla"),"relax":counts.get::<i64,_>("relax")},
        "by_mode": [counts.get::<i64,_>("m0"),counts.get::<i64,_>("m1"),counts.get::<i64,_>("m2"),counts.get::<i64,_>("m3")],
        "current_versions": {"vanilla":scores::PP_VERSION_VANILLA,"relax":scores::PP_VERSION_RELAX},
        "sample": sample.into_iter().map(|row| json!({
            "id":row.get::<i64,_>("id"),"user_id":row.get::<i64,_>("user_id"),"username":row.get::<String,_>("username"),
            "title":row.get::<String,_>("title"),"artist":row.get::<String,_>("artist"),"version":row.get::<String,_>("version"),
            "beatmap_id":row.get::<i64,_>("native_id"),"mode":row.get::<i16,_>("mode"),"score_variant":row.get::<String,_>("score_variant"),
            "pp":row.get::<f64,_>("pp"),"pp_calculation_version":row.get::<Option<String>,_>("pp_calculation_version"),
            "created_at":row.get::<chrono::DateTime<chrono::Utc>,_>("created_at"),
        })).collect::<Vec<_>>(),
    })))
}

const JOB_JSON: &str = "SELECT to_jsonb(j)||jsonb_build_object('created_by_name',u.username) FROM score_recalculation_jobs j JOIN users u ON u.id=j.created_by";

pub async fn create_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PreviewInput>,
) -> ApiResult {
    let snapshot = require_operator(&state, &headers).await?;
    let filters = resolve(&state.db, input.filters).await?;
    let total: i64 = bind_match(
        sqlx::query(&format!("SELECT count(*)::bigint {MATCH}")),
        &filters,
    )
    .fetch_one(&state.db)
    .await?
    .get(0);
    if total == 0 {
        return Err(ApiError::bad("No scores match these filters."));
    }
    let mut tx = state.db.begin().await?;
    let operator =
        auth::mutation_actor(&mut tx, &snapshot, &[], auth::MutationScope::Permissions).await?;
    operator.require("scores.moderate")?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO score_recalculation_jobs(created_by,filters,total) VALUES($1,$2,$3) RETURNING id",
    )
    .bind(operator.id)
    .bind(json!(filters))
    .bind(total)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| {
        if error
            .as_database_error()
            .is_some_and(|e| e.is_unique_violation())
        {
            ApiError(
                StatusCode::CONFLICT,
                "recalculation_active",
                "Another recalculation is already queued or running. Wait for it or cancel it.".into(),
            )
        } else {
            error.into()
        }
    })?;
    sqlx::query("INSERT INTO audit_events(actor_id,action,target_type,target_id,details) VALUES($1,'scores.recalculate_start','score_recalculation_job',$2,$3)")
        .bind(operator.id)
        .bind(id.to_string())
        .bind(json!({"filters":filters,"total":total}))
        .execute(&mut *tx)
        .await?;
    auth::ensure_mutation_actor(&mut tx, &operator).await?;
    tx.commit().await?;
    job_json(&state.db, id).await
}

async fn job_json(db: &PgPool, id: i64) -> ApiResult {
    let value: Value = sqlx::query_scalar(&format!("{JOB_JSON} WHERE j.id=$1"))
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or_else(ApiError::missing)?;
    Ok(Json(value))
}

pub async fn list_jobs(State(state): State<AppState>, headers: HeaderMap) -> ApiResult {
    require_operator(&state, &headers).await?;
    let items: Vec<Value> = sqlx::query_scalar(&format!("{JOB_JSON} ORDER BY j.id DESC LIMIT 20"))
        .fetch_all(&state.db)
        .await?;
    let total = items.len() as i64;
    Ok(crate::page(items, total, 1, total.max(1)))
}

pub async fn job_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult {
    require_operator(&state, &headers).await?;
    let Json(mut job) = job_json(&state.db, id).await?;
    let item_rows = |outcomes: &'static str, order: &'static str| {
        format!(
            "SELECT i.score_id,i.outcome,i.previous_pp,i.new_pp,i.detail,u.username,b.title,b.version,b.native_id FROM score_recalculation_items i JOIN scores s ON s.id=i.score_id JOIN users u ON u.id=s.user_id JOIN beatmaps b ON b.id=s.beatmap_id WHERE i.job_id=$1 AND i.outcome IN ({outcomes}) ORDER BY {order} LIMIT 50"
        )
    };
    let to_json = |row: PgRow| {
        json!({
            "score_id":row.get::<i64,_>("score_id"),"outcome":row.get::<String,_>("outcome"),
            "previous_pp":row.get::<Option<f64>,_>("previous_pp"),"new_pp":row.get::<Option<f64>,_>("new_pp"),
            "detail":row.get::<Option<String>,_>("detail"),"username":row.get::<String,_>("username"),
            "title":row.get::<String,_>("title"),"version":row.get::<String,_>("version"),"beatmap_id":row.get::<i64,_>("native_id"),
        })
    };
    let changes = sqlx::query(&item_rows(
        "'updated'",
        "abs(COALESCE(i.new_pp,0)-COALESCE(i.previous_pp,0)) DESC,i.score_id",
    ))
    .bind(id)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(to_json)
    .collect::<Vec<_>>();
    let problems = sqlx::query(&item_rows("'skipped','failed'", "i.outcome,i.score_id"))
        .bind(id)
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .map(to_json)
        .collect::<Vec<_>>();
    let delta: f64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(new_pp-previous_pp),0)::float8 FROM score_recalculation_items WHERE job_id=$1 AND outcome='updated'",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    job["changes"] = json!(changes);
    job["problems"] = json!(problems);
    job["pp_delta"] = json!(delta);
    Ok(Json(job))
}

pub async fn cancel_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult {
    let snapshot = require_operator(&state, &headers).await?;
    let mut tx = state.db.begin().await?;
    let operator =
        auth::mutation_actor(&mut tx, &snapshot, &[], auth::MutationScope::Permissions).await?;
    operator.require("scores.moderate")?;
    let changed = sqlx::query(
        "UPDATE score_recalculation_jobs SET cancel_requested=true,
            status=CASE WHEN status='queued' THEN 'cancelled' ELSE status END,
            finished_at=CASE WHEN status='queued' THEN now() ELSE finished_at END
         WHERE id=$1 AND status IN ('queued','running')",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed == 0 {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM score_recalculation_jobs WHERE id=$1)")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        return Err(if exists {
            ApiError(
                StatusCode::CONFLICT,
                "recalculation_finished",
                "This recalculation has already finished.".into(),
            )
        } else {
            ApiError::missing()
        });
    }
    sqlx::query("INSERT INTO audit_events(actor_id,action,target_type,target_id,details) VALUES($1,'scores.recalculate_cancel','score_recalculation_job',$2,'{}')")
        .bind(operator.id)
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
    auth::ensure_mutation_actor(&mut tx, &operator).await?;
    tx.commit().await?;
    job_json(&state.db, id).await
}

enum Outcome {
    Updated { previous: f64, new: f64 },
    Unchanged,
    Skipped(&'static str),
}

/// Recalculate one stored score. The write happens under a row lock after the
/// stored values are compared with the ones the calculation used.
async fn recalculate_one(state: &AppState, row: &PgRow) -> ApiResult<Outcome> {
    let id: i64 = row.get("id");
    let client: String = row.get("client");
    let submission = scores::from_stored_row(row)?;
    let (variant, eligible) = scores::mod_partition(&submission, &client)?;
    if variant.as_str() != row.get::<String, _>("score_variant") {
        return Ok(Outcome::Skipped(
            "The mods no longer match the stored score variant.",
        ));
    }
    if !eligible {
        return Ok(Outcome::Skipped("These mods are not eligible for pp."));
    }
    crate::score_scales::validate_native_binding(&submission)?;
    let path: String = row.get("osu_path");
    if std::path::Path::new(&path).is_absolute() || path.contains("..") {
        return Err(ApiError::internal("unsafe stored map path"));
    }
    let bytes = tokio::fs::read(state.maps_root.join(path))
        .await
        .map_err(|_| ApiError::bad("The beatmap file is missing."))?;
    let lazer = client == "lazer";
    let calculated =
        tokio::task::spawn_blocking(move || scores::calculate_score(&bytes, &submission, lazer))
            .await
            .map_err(ApiError::internal)??;
    let binding = scores::relax_calculation_binding(row);

    let mut tx = state.db.begin().await?;
    let current = sqlx::query("SELECT s.*,b.osu_path FROM scores s JOIN beatmaps b ON b.id=s.beatmap_id JOIN users u ON u.id=s.user_id WHERE s.id=$1 AND NOT u.is_restricted FOR UPDATE OF s")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(current) = current else {
        return Ok(Outcome::Skipped("The score is no longer available."));
    };
    if scores::relax_calculation_binding(&current) != binding
        || !matches!(
            current.get::<String, _>("verification").as_str(),
            "pending" | "verified"
        )
    {
        return Ok(Outcome::Skipped(
            "The score changed while it was being calculated.",
        ));
    }
    let previous: f64 = current.get("pp");
    let previous_eligible: bool = current.get("pp_eligible");
    sqlx::query("UPDATE scores SET pp=$2,pp_eligible=true,pp_calculation_version=$3 WHERE id=$1")
        .bind(id)
        .bind(calculated.pp)
        .bind(scores::pp_version(variant))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(
        if (calculated.pp - previous).abs() > PP_EPSILON || !previous_eligible {
            Outcome::Updated {
                previous,
                new: calculated.pp,
            }
        } else {
            Outcome::Unchanged
        },
    )
}

async fn operator_authorized(db: &PgPool, user_id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users u WHERE u.id=$1 AND NOT u.is_restricted AND EXISTS(
            SELECT 1 FROM user_roles ur JOIN roles r ON r.id=ur.role_id
            WHERE ur.user_id=u.id AND ('*'=ANY(r.permissions) OR 'scores.moderate'=ANY(r.permissions))))",
    )
    .bind(user_id)
    .fetch_one(db)
    .await
}

async fn finish(
    state: &AppState,
    id: i64,
    status: &str,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    let row = sqlx::query("UPDATE score_recalculation_jobs SET status=$2,error=$3,finished_at=now() WHERE id=$1 RETURNING created_by,total,processed,updated,unchanged,skipped,failed,filters")
        .bind(id)
        .bind(status)
        .bind(error)
        .fetch_one(&state.db)
        .await?;
    sqlx::query("INSERT INTO audit_events(actor_id,action,target_type,target_id,details) VALUES($1,'scores.recalculate_finish','score_recalculation_job',$2,$3)")
        .bind(row.get::<i64, _>("created_by"))
        .bind(id.to_string())
        .bind(json!({
            "status":status,"error":error,"total":row.get::<i64,_>("total"),
            "processed":row.get::<i64,_>("processed"),"updated":row.get::<i64,_>("updated"),
            "unchanged":row.get::<i64,_>("unchanged"),"skipped":row.get::<i64,_>("skipped"),
            "failed":row.get::<i64,_>("failed"),"filters":row.get::<Value,_>("filters"),
        }))
        .execute(&state.db)
        .await?;
    Ok(())
}

fn short(text: &str) -> String {
    text.chars().take(280).collect()
}

async fn run_job(state: &AppState, id: i64) -> Result<(), sqlx::Error> {
    loop {
        let job = sqlx::query(
            "SELECT created_by,filters,last_score_id,cancel_requested FROM score_recalculation_jobs WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&state.db)
        .await?;
        if job.get::<bool, _>("cancel_requested") {
            return finish(state, id, "cancelled", None).await;
        }
        if !operator_authorized(&state.db, job.get("created_by")).await? {
            return finish(
                state,
                id,
                "failed",
                Some("The operator no longer has permission to moderate scores."),
            )
            .await;
        }
        let Ok(filters) = serde_json::from_value::<Filters>(job.get("filters")) else {
            return finish(state, id, "failed", Some("The stored filters are invalid.")).await;
        };
        let rows = bind_match(
            sqlx::query(&format!(
                "SELECT s.*,b.osu_path {MATCH} AND s.id>$14 ORDER BY s.id LIMIT {BATCH}"
            )),
            &filters,
        )
        .bind(job.get::<i64, _>("last_score_id"))
        .fetch_all(&state.db)
        .await?;
        if rows.is_empty() {
            return finish(state, id, "completed", None).await;
        }
        let (mut updated, mut unchanged, mut skipped, mut failed) = (0_i64, 0_i64, 0_i64, 0_i64);
        let mut last = 0_i64;
        for row in &rows {
            let score_id: i64 = row.get("id");
            last = score_id;
            let (outcome, previous, new, detail) = match recalculate_one(state, row).await {
                Ok(Outcome::Updated { previous, new }) => {
                    updated += 1;
                    ("updated", Some(previous), Some(new), None)
                }
                Ok(Outcome::Unchanged) => {
                    unchanged += 1;
                    continue;
                }
                Ok(Outcome::Skipped(reason)) => {
                    skipped += 1;
                    ("skipped", None, None, Some(reason.to_owned()))
                }
                Err(error) => {
                    failed += 1;
                    ("failed", None, None, Some(short(&error.2)))
                }
            };
            sqlx::query("INSERT INTO score_recalculation_items(job_id,score_id,outcome,previous_pp,new_pp,detail) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING")
                .bind(id)
                .bind(score_id)
                .bind(outcome)
                .bind(previous)
                .bind(new)
                .bind(detail)
                .execute(&state.db)
                .await?;
        }
        sqlx::query("UPDATE score_recalculation_jobs SET processed=processed+$2,updated=updated+$3,unchanged=unchanged+$4,skipped=skipped+$5,failed=failed+$6,last_score_id=$7 WHERE id=$1")
            .bind(id)
            .bind(rows.len() as i64)
            .bind(updated)
            .bind(unchanged)
            .bind(skipped)
            .bind(failed)
            .bind(last)
            .execute(&state.db)
            .await?;
        // Leave the database and CPU to player traffic between batches.
        tokio::time::sleep(BATCH_PAUSE).await;
    }
}

/// Run the oldest queued or interrupted job. Returns whether one was found.
pub(crate) async fn run_next(state: &AppState) -> Result<bool, sqlx::Error> {
    let claimed = sqlx::query_scalar::<_, i64>(
        "UPDATE score_recalculation_jobs SET status='running',started_at=COALESCE(started_at,now())
         WHERE id=(SELECT id FROM score_recalculation_jobs WHERE status IN ('queued','running') ORDER BY id LIMIT 1)
         RETURNING id",
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(id) = claimed else {
        return Ok(false);
    };
    tracing::info!(
        code = "score_recalculation_started",
        job = id,
        "bulk recalculation started"
    );
    if let Err(error) = run_job(state, id).await {
        tracing::warn!(code = "score_recalculation_failed", job = id, %error, "bulk recalculation stopped");
        // A broken connection also breaks this write; the job is then resumed after restart.
        let _ = finish(
            state,
            id,
            "failed",
            Some("The recalculation stopped after a database error."),
        )
        .await;
    }
    Ok(true)
}

pub(crate) fn start(state: AppState) {
    tokio::spawn(async move {
        loop {
            match run_next(&state).await {
                Ok(true) => {}
                Ok(false) => tokio::time::sleep(IDLE_POLL).await,
                Err(error) => {
                    tracing::warn!(code = "score_recalculation_poll_failed", %error, "bulk recalculation poll failed");
                    tokio::time::sleep(ERROR_POLL).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> FilterInput {
        FilterInput::default()
    }

    #[test]
    fn filters_reject_unknown_values() {
        let mut bad = input();
        bad.variant = Some("autopilot".into());
        assert!(validate(&bad).is_err());
        let mut bad = input();
        bad.mode = Some(4);
        assert!(validate(&bad).is_err());
        let mut bad = input();
        bad.verification = Some("rejected".into());
        assert!(validate(&bad).is_err());
    }

    #[test]
    fn date_and_pp_ranges_must_be_ordered() {
        let day = |d| NaiveDate::from_ymd_opt(2026, 10, d).unwrap();
        let mut dates = input();
        dates.from = Some(day(10));
        dates.to = Some(day(9));
        assert!(validate(&dates).is_err());
        dates.to = Some(day(10));
        assert!(validate(&dates).is_ok());
        let mut pp = input();
        pp.min_pp = Some(200.0);
        pp.max_pp = Some(100.0);
        assert!(validate(&pp).is_err());
        assert!(finite_pp(Some(f64::NAN), "pp").is_err());
        assert!(finite_pp(Some(-1.0), "pp").is_err());
    }

    #[test]
    fn explicit_score_ids_are_bounded_unique_and_positive() {
        for ids in [vec![], vec![1, 1], vec![0], vec![-3]] {
            let mut bad = input();
            bad.score_ids = Some(ids);
            assert!(validate(&bad).is_err());
        }
        let mut too_many = input();
        too_many.score_ids = Some((1..=(MAX_SCORE_IDS as i64 + 1)).collect());
        assert!(validate(&too_many).is_err());
        let mut fine = input();
        fine.score_ids = Some(vec![5, 2, 9]);
        assert!(validate(&fine).is_ok());
    }

    #[test]
    fn unknown_filter_fields_are_refused() {
        assert!(serde_json::from_value::<FilterInput>(json!({"nope":1})).is_err());
        assert!(
            serde_json::from_value::<FilterInput>(json!({"from":"2026-10-01","mode":0})).is_ok()
        );
    }

    #[test]
    fn error_text_is_truncated() {
        assert_eq!(short(&"x".repeat(1000)).chars().count(), 280);
    }

    #[test]
    fn every_variant_has_a_current_version() {
        assert_ne!(
            scores::pp_version(ScoreVariant::Relax),
            scores::pp_version(ScoreVariant::Vanilla)
        );
    }
}
