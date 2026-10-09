//! Account-wide timed chat policy. User locks serialize moderation with every
//! new outgoing website/native message; accepted retry identities remain valid.
use crate::{ApiError, ApiResult, AppState, auth, page};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use chrono::{DateTime, Utc};
use illya_domain::{Pagination, has_permission};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};

#[cfg(test)]
#[path = "chat_moderation_tests.rs"]
mod integration_tests;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SilenceInput {
    expires_at: DateTime<Utc>,
    reason: String,
    expected_revision: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiftInput {
    reason: String,
    expected_revision: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryQuery {
    page: Option<u32>,
    page_size: Option<u32>,
    search: Option<String>,
}

fn validate_change(reason: &str, revision: i64) -> ApiResult<&str> {
    if reason.chars().any(char::is_control) {
        return Err(ApiError::bad(
            "Give a reason of 1–500 characters without control characters.",
        ));
    }
    let reason = reason.trim();
    if reason.is_empty() || reason.chars().count() > 500 {
        return Err(ApiError::bad(
            "Give a reason of 1–500 characters without control characters.",
        ));
    }
    if revision < 0 {
        return Err(ApiError::bad("Chat policy revision must be non-negative."));
    }
    Ok(reason)
}

fn may_silence(actor: &auth::User, target: i64, target_permissions: &[String]) -> bool {
    target != actor.id
        && has_permission(&actor.permissions, "chat.moderate")
        && (actor.permissions.iter().any(|permission| permission == "*")
            || !has_permission(target_permissions, "chat.moderate"))
}

async fn validate_expiry(
    tx: &mut Transaction<'_, Postgres>,
    expiry: DateTime<Utc>,
) -> ApiResult<()> {
    // Evaluate after all locks, including waits on the actor's session. `now()`
    // is the transaction start and would admit a deadline that already passed.
    let valid: bool = sqlx::query_scalar(
        "WITH clock AS MATERIALIZED(SELECT clock_timestamp() AS observed_at) SELECT $1::timestamptz>observed_at AND $1<=observed_at+interval '30 days' FROM clock",
    )
    .bind(expiry)
    .fetch_one(&mut **tx)
    .await?;
    if !valid {
        return Err(ApiError::bad(
            "Chat silence must end in the future, within 30 days.",
        ));
    }
    Ok(())
}

pub async fn apply(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<SilenceInput>,
) -> ApiResult {
    let reason = validate_change(&input.reason, input.expected_revision)?;
    change(
        &state,
        &headers,
        id,
        Some(input.expires_at),
        reason,
        input.expected_revision,
    )
    .await
}

pub async fn lift(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<LiftInput>,
) -> ApiResult {
    let reason = validate_change(&input.reason, input.expected_revision)?;
    change(&state, &headers, id, None, reason, input.expected_revision).await
}

async fn change(
    state: &AppState,
    headers: &HeaderMap,
    target: i64,
    expiry: Option<DateTime<Utc>>,
    reason: &str,
    expected_revision: i64,
) -> ApiResult {
    let snapshot = auth::require(state, headers).await?;
    snapshot.require("chat.moderate")?;
    if target == snapshot.id {
        return Err(ApiError::bad("You cannot change your own chat silence."));
    }
    let mut tx = state.db.begin().await?;
    let actor = auth::mutation_actor(
        &mut tx,
        &snapshot,
        &[target],
        auth::MutationScope::Permissions,
    )
    .await?;
    actor.require("chat.moderate")?;
    let previous: Value = sqlx::query_scalar("SELECT jsonb_build_object('chat_silenced_until',chat_silenced_until,'chat_silence_revision',chat_silence_revision) FROM users WHERE id=$1")
        .bind(target)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::missing)?;
    let permissions: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT unnest(r.permissions) FROM roles r JOIN user_roles ur ON ur.role_id=r.id WHERE ur.user_id=$1",
    )
    .bind(target)
    .fetch_all(&mut *tx)
    .await?;
    if !may_silence(&actor, target, &permissions) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "protected_chat_operator",
            "Only a full administrator can change another chat moderator's silence.".into(),
        ));
    }
    if previous["chat_silence_revision"].as_i64() != Some(expected_revision) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "chat_silence_conflict",
            "This player's chat policy changed after you loaded it. Refresh before changing it again.".into(),
        ));
    }
    if let Some(expiry) = expiry {
        validate_expiry(&mut tx, expiry).await?;
    }
    let updated: Value = sqlx::query_scalar("UPDATE users SET chat_silenced_until=$2 WHERE id=$1 RETURNING jsonb_build_object('chat_silenced_until',chat_silenced_until,'chat_silence_revision',chat_silence_revision)")
        .bind(target)
        .bind(expiry)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO audit_events(actor_id,action,target_type,target_id,details) VALUES($1,$2,'user',$3,$4)")
        .bind(actor.id)
        .bind(if expiry.is_some() { "chat.silence" } else { "chat.silence.lift" })
        .bind(target.to_string())
        .bind(json!({"previous":previous,"new":updated,"reason":reason}))
        .execute(&mut *tx)
        .await?;
    // Only identifiers enter the committed refresh stream, never the reason.
    sqlx::query("SELECT pg_notify('illya_chat',json_build_object('schema',current_schema(),'kind','chat_policy_changed','user',$1::bigint)::text)")
        .bind(target)
        .execute(&mut *tx)
        .await?;
    if let Some(expiry) = expiry {
        validate_expiry(&mut tx, expiry).await?;
    }
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    let server_time: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "ok":true,"id":target,"chat_silenced_until":expiry.filter(|until| *until>server_time),
        "chat_silence_revision":updated["chat_silence_revision"],"server_time":server_time
    })))
}

pub async fn directory(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DirectoryQuery>,
) -> ApiResult {
    let snapshot = auth::require(&state, &headers).await?;
    snapshot.require("chat.moderate")?;
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if search.is_some_and(|s| s.chars().count() > 64 || s.chars().any(char::is_control)) {
        return Err(ApiError::bad(
            "Search must be at most 64 characters without control characters.",
        ));
    }
    let search_id = search
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|id| *id > 0);
    let (p, size, offset) = Pagination {
        page: query.page,
        page_size: query.page_size,
    }
    .bounds();
    let mut tx = state.db.begin().await?;
    let actor =
        auth::mutation_actor(&mut tx, &snapshot, &[], auth::MutationScope::Permissions).await?;
    actor.require("chat.moderate")?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM users u WHERE ($1::text IS NULL OR ($2::bigint IS NOT NULL AND u.id=$2) OR ($2::bigint IS NULL AND strpos(lower(u.username),lower($1))>0))")
        .bind(search).bind(search_id).fetch_one(&mut *tx).await?;
    let server_time: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let items = sqlx::query_scalar::<_,Value>(r#"
        SELECT jsonb_build_object(
            'id',u.id,'username',u.username,'country',u.country,'avatar_url',u.avatar_url,
            'chat_silenced_until',CASE WHEN u.chat_silenced_until>$7 THEN u.chat_silenced_until END,
            'chat_silence_revision',u.chat_silence_revision,
            'can_silence',u.id<>$5 AND (NOT protected.value OR '*'=ANY($6)),
            'is_protected',protected.value
        ) FROM users u CROSS JOIN LATERAL (
            SELECT EXISTS(SELECT 1 FROM roles r JOIN user_roles ur ON ur.role_id=r.id
                WHERE ur.user_id=u.id AND ('*'=ANY(r.permissions) OR 'chat.moderate'=ANY(r.permissions))) value
        ) protected
        WHERE ($3::text IS NULL OR ($4::bigint IS NOT NULL AND u.id=$4)
            OR ($4::bigint IS NULL AND strpos(lower(u.username),lower($3))>0))
        ORDER BY u.id LIMIT $1 OFFSET $2
        "#)
        .bind(size).bind(offset).bind(search).bind(search_id).bind(actor.id)
        .bind(&actor.permissions).bind(server_time).fetch_all(&mut *tx).await?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    tx.commit().await?;
    let mut value = page(items, total, p, size).0;
    value["server_time"] = json!(server_time);
    Ok(Json(value))
}

#[derive(Serialize)]
pub(crate) struct SendingStatus {
    pub(crate) can_send: bool,
    pub(crate) send_restriction: Option<&'static str>,
    pub(crate) silenced_until: Option<DateTime<Utc>>,
    server_time: DateTime<Utc>,
}

pub(crate) async fn sending_status(
    tx: &mut Transaction<'_, Postgres>,
    user: &auth::User,
) -> ApiResult<SendingStatus> {
    let row = sqlx::query(
        "SELECT chat_silenced_until,clock_timestamp() AS server_time FROM users WHERE id=$1",
    )
    .bind(user.id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(ApiError::missing)?;
    let server_time: DateTime<Utc> = row.get("server_time");
    let silenced_until = row
        .get::<Option<DateTime<Utc>>, _>("chat_silenced_until")
        .filter(|until| *until > server_time);
    let send_restriction = if !has_permission(&user.permissions, "chat.send") {
        Some("no_permission")
    } else if silenced_until.is_some() {
        Some("silenced")
    } else {
        None
    };
    Ok(SendingStatus {
        can_send: send_restriction.is_none(),
        send_restriction,
        silenced_until,
        server_time,
    })
}

/// The caller must already hold the sender user row and a permission fence.
/// Invoke only after matching an accepted UUID retry, immediately before INSERT.
pub(crate) async fn require_new_message(
    tx: &mut Transaction<'_, Postgres>,
    actor: &auth::User,
) -> ApiResult<()> {
    let status = sending_status(tx, actor).await?;
    if let Some(until) = status.silenced_until {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "chat_silenced",
            format!(
                "Chat is silenced until {}.",
                until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ),
        ));
    }
    actor.require("chat.send")
}

pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> ApiResult {
    let snapshot = auth::require(&state, &headers).await?;
    let mut tx = state.db.begin().await?;
    let actor =
        auth::mutation_actor(&mut tx, &snapshot, &[], auth::MutationScope::Permissions).await?;
    let value = sending_status(&mut tx, &actor).await?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    tx.commit().await?;
    Ok(Json(json!(value)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn strict_changes_require_reason_revision_and_rfc3339_expiry() {
        for value in [
            json!({"reason":"reason","expected_revision":0}),
            json!({"expires_at":"tomorrow","reason":"reason","expected_revision":0}),
            json!({"expires_at":"2026-10-10T00:00:00Z","reason":"reason"}),
            json!({"expires_at":"2026-10-10T00:00:00Z","reason":"reason","expected_revision":0,"user_id":1}),
        ] {
            assert!(serde_json::from_value::<SilenceInput>(value).is_err());
        }
        assert!(
            serde_json::from_value::<LiftInput>(
                json!({"reason":"x","expected_revision":0,"expires_at":null})
            )
            .is_err()
        );
        for reason in ["", "   ", "line\nbreak", "\nreason\n", "control\0"] {
            assert!(validate_change(reason, 0).is_err());
        }
        assert!(validate_change(&"x".repeat(501), 0).is_err());
        assert!(validate_change("reason", -1).is_err());
        assert_eq!(validate_change(" reason ", 0).unwrap(), "reason");
    }

    #[test]
    fn chat_only_moderators_can_act_on_players_but_not_peers_or_themselves() {
        let actor = auth::User {
            id: 1,
            session: Uuid::nil(),
            permissions: vec!["chat.moderate".into()],
        };
        assert!(may_silence(&actor, 2, &["chat.send".into()]));
        assert!(!may_silence(&actor, 1, &[]));
        assert!(!may_silence(&actor, 2, &["chat.moderate".into()]));
        assert!(!may_silence(&actor, 2, &["*".into()]));
        let admin = auth::User {
            permissions: vec!["*".into()],
            ..actor
        };
        assert!(may_silence(&admin, 2, &["chat.moderate".into()]));
        assert!(may_silence(&admin, 2, &["*".into()]));
        assert!(!may_silence(&admin, 1, &[]));
    }
}
