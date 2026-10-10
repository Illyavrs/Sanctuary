//! Resolve public local set URLs without exposing storage or internal row IDs.
use crate::{ApiError, ApiResult, AppState, maps};
use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::Deserialize;

#[derive(Default, Deserialize)]
pub(crate) struct Selection {
    difficulty: Option<i64>,
    mode: Option<i16>,
}

pub(crate) async fn detail(
    State(state): State<AppState>,
    Path(set_id): Path<i64>,
    Query(selection): Query<Selection>,
) -> ApiResult {
    if !(2_000_000_000..=i64::from(i32::MAX)).contains(&set_id)
        || selection
            .difficulty
            .is_some_and(|id| !(1_000_000_000..=i64::from(i32::MAX)).contains(&id))
        || selection.mode.is_some_and(|mode| !(0..=3).contains(&mode))
    {
        return Err(ApiError::bad("Invalid local beatmap identity."));
    }
    let id: i64 = sqlx::query_scalar(
        "SELECT id FROM beatmaps WHERE native_set_id=$1 AND source='local' \
         AND ($2::bigint IS NULL OR native_id=$2) \
         AND ($3::smallint IS NULL OR mode=$3) ORDER BY id LIMIT 1",
    )
    .bind(set_id)
    .bind(selection.difficulty)
    .bind(selection.mode)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(ApiError::missing)?;
    // The existing public projection applies the active-set boundary and keeps
    // current per-difficulty status, membership, and download authority intact.
    Ok(Json(maps::get(&state, id).await?))
}
