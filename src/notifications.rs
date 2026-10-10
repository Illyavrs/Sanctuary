//! Durable, owner-only system notices. Reading the bell never reads chat.
use crate::{ApiError, ApiResult, AppState, auth};
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

const MAX_IDS: usize = 100;
const MARK_ALL_BATCH: i64 = 1000;
const MAX_SNAPSHOT_BYTES: usize = 96 * 1024;
const SNAPSHOT_LIFETIME_SECONDS: i64 = 900;

#[derive(Deserialize, Serialize)]
struct Snapshot {
    purpose: String,
    sub: i64,
    sid: Uuid,
    rows: Vec<(i64, Uuid)>,
    exp: usize,
}

// Derive a separate signing key: these scoped acknowledgements must never be
// accepted as website bearer credentials, even if their prefix is stripped.
fn snapshot_secret(state: &AppState) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(b"Illyaverse notification read snapshot v1\0");
    hash.update(state.secret.as_bytes());
    hash.finalize().to_vec()
}

fn encode_snapshot(
    state: &AppState,
    user: &auth::User,
    rows: Vec<(i64, Uuid)>,
) -> ApiResult<String> {
    let value = Snapshot {
        purpose: "notification_read".into(),
        sub: user.id,
        sid: user.session,
        rows,
        exp: (chrono::Utc::now().timestamp() + SNAPSHOT_LIFETIME_SECONDS) as usize,
    };
    let encoded = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &value,
        &jsonwebtoken::EncodingKey::from_secret(&snapshot_secret(state)),
    )
    .map_err(ApiError::internal)?;
    Ok(format!("notification-snapshot.{encoded}"))
}

fn decode_snapshot(state: &AppState, user: &auth::User, token: &str) -> ApiResult<Snapshot> {
    let invalid = || ApiError::bad("Refresh notifications before marking this snapshot as read.");
    let token = token
        .strip_prefix("notification-snapshot.")
        .ok_or_else(invalid)?;
    if token.len() > MAX_SNAPSHOT_BYTES {
        return Err(invalid());
    }
    let mut validation = jsonwebtoken::Validation::default();
    validation.leeway = 0;
    let value = jsonwebtoken::decode::<Snapshot>(
        token,
        &jsonwebtoken::DecodingKey::from_secret(&snapshot_secret(state)),
        &validation,
    )
    .map_err(|_| invalid())?
    .claims;
    if value.purpose != "notification_read"
        || value.sub != user.id
        || value.sid != user.session
        || value.rows.len() > MARK_ALL_BATCH as usize
        || value.rows.iter().any(|(id, _)| *id <= 0)
        || value
            .rows
            .iter()
            .map(|(id, _)| *id)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != value.rows.len()
    {
        return Err(invalid());
    }
    ensure_snapshot_live(&value)?;
    Ok(value)
}

fn ensure_snapshot_live(value: &Snapshot) -> ApiResult<()> {
    if value.exp <= chrono::Utc::now().timestamp() as usize {
        Err(ApiError::bad(
            "Refresh notifications before marking this snapshot as read.",
        ))
    } else {
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct List {
    page: Option<u32>,
    page_size: Option<u32>,
}
impl List {
    fn bounds(&self) -> (i64, i64, i64) {
        let page = i64::from(self.page.unwrap_or(1).clamp(1, 10_000));
        let size = i64::from(self.page_size.unwrap_or(20).clamp(1, 50));
        (page, size, (page - 1) * size)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Read {
    ids: Option<Vec<i64>>,
    through_id: Option<String>,
}
impl Read {
    fn bounds(&self) -> ApiResult<(Option<&[i64]>, Option<&str>)> {
        match (&self.ids, self.through_id.as_deref()) {
            (Some(ids), None)
                if (1..=MAX_IDS).contains(&ids.len())
                    && ids.iter().all(|id| *id > 0)
                    && ids.iter().collect::<std::collections::BTreeSet<_>>().len() == ids.len() =>
            {
                Ok((Some(ids.as_slice()), None))
            }
            (None, Some(token)) if !token.is_empty() && token.len() <= MAX_SNAPSHOT_BYTES => {
                Ok((None, Some(token)))
            }
            _ => Err(ApiError::bad(
                "Supply 1–100 unique notification IDs or one notification read snapshot.",
            )),
        }
    }
}

// Announcement edits preserve publication_at. Withdrawal hides the original
// event immediately; republishing creates a new epoch, so an old event cannot
// reappear. Rank-request links and ownership come from its immutable source.
const ELIGIBLE: &str = r#"(
    (n.kind IN ('rank_request_approved','rank_request_rejected') AND EXISTS (
        SELECT 1 FROM beatmap_rank_requests r
        WHERE r.id=n.rank_request_id AND r.requester_id=n.user_id
            AND n.kind='rank_request_' || r.status))
    OR (n.kind='announcement_published' AND EXISTS (
        SELECT 1 FROM announcements a JOIN users recipient ON recipient.id=n.user_id
        WHERE a.id=n.announcement_id AND a.status='published'
            AND a.published_at=n.publication_at AND recipient.joined_at<=n.publication_at))
)"#;

pub(crate) async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<List>,
) -> ApiResult<Response> {
    let snapshot = auth::require(&state, &headers).await?;
    let (page, size, offset) = query.bounds();
    let mut tx = state.db.begin().await?;
    // The credential/session/restriction fences also protect GET delivery from
    // revocation while waiting. No notification can authorize another resource.
    let actor = auth::mutation_actor(&mut tx, &snapshot, &[], auth::MutationScope::Session).await?;
    let sql = format!(
        r#"WITH visible AS MATERIALIZED (
            SELECT n.* FROM user_notifications n WHERE n.user_id=$1 AND {ELIGIBLE}
        ), selected AS (SELECT * FROM visible ORDER BY id DESC LIMIT $2 OFFSET $3)
        SELECT COALESCE((SELECT jsonb_agg(jsonb_build_object(
            'id',n.id,'kind',n.kind,'title',n.title,'message',n.message,
            'href',CASE WHEN n.kind='announcement_published' THEN '/news/' || n.announcement_id
                ELSE (SELECT '/beatmapsets/' || CASE WHEN r.source='local' THEN 'local-' ELSE '' END || r.native_set_id
                    FROM beatmap_rank_requests r WHERE r.id=n.rank_request_id) END,
            'created_at',n.created_at,'read_at',n.read_at) ORDER BY n.id DESC)
            FROM selected n),'[]'::jsonb) AS items,
            (SELECT count(*) FROM visible) AS total,
            (SELECT count(*) FROM visible WHERE read_at IS NULL) AS unread_count,
            COALESCE((SELECT jsonb_agg(jsonb_build_array(id,identity) ORDER BY id) FROM (
                SELECT id,identity FROM visible WHERE read_at IS NULL ORDER BY id LIMIT 1000
            ) observed),'[]'::jsonb) AS observed_rows"#,
    );
    let row = sqlx::query(&sql)
        .bind(actor.id)
        .bind(size)
        .bind(offset)
        .fetch_one(&mut *tx)
        .await?;
    let observed_rows: Vec<(i64, Uuid)> =
        serde_json::from_value(row.try_get("observed_rows")?).map_err(ApiError::internal)?;
    let value = json!({
        "items":row.try_get::<Value,_>("items")?,"total":row.try_get::<i64,_>("total")?,
        "page":page,"page_size":size,"unread_count":row.try_get::<i64,_>("unread_count")?,
        "snapshot_size":observed_rows.len(),
        "snapshot_id":encode_snapshot(&state,&actor,observed_rows)?
    });
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    tx.commit().await?;
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(value)).into_response())
}

pub(crate) async fn mark_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Read>,
) -> ApiResult<Response> {
    let snapshot = auth::require(&state, &headers).await?;
    let (ids, token) = input.bounds()?;
    let mut tx = state.db.begin().await?;
    let actor = auth::mutation_actor(&mut tx, &snapshot, &[], auth::MutationScope::Session).await?;
    let snapshot = token
        .map(|token| decode_snapshot(&state, &actor, token))
        .transpose()?;
    let (snapshot_ids, snapshot_identities): (Vec<i64>, Vec<Uuid>) = snapshot
        .as_ref()
        .map(|value| value.rows.iter().copied().unzip())
        .unwrap_or_default();
    let selected_ids = ids.or_else(|| snapshot.as_ref().map(|_| snapshot_ids.as_slice()));
    let selected_identities = snapshot.as_ref().map(|_| snapshot_identities.as_slice());
    // Sequence allocation is not commit order. The signed token names actual
    // observed row identities, so later-committing lower IDs and sequence IDs
    // reused after restoration remain unread. One token covers at most 1000
    // rows; further work needs a fresh explicit read.
    let sql = format!(
        r#"WITH selected AS (
            SELECT n.id FROM user_notifications n WHERE n.user_id=$1 AND n.read_at IS NULL
                AND {ELIGIBLE} AND n.id=ANY($2::bigint[])
                AND ($4::uuid[] IS NULL OR EXISTS (
                    SELECT 1 FROM unnest($2::bigint[],$4::uuid[]) observed(id,identity)
                    WHERE observed.id=n.id AND observed.identity=n.identity))
            ORDER BY n.id LIMIT $3 FOR UPDATE OF n
        ) UPDATE user_notifications n SET read_at=clock_timestamp()
            FROM selected s WHERE n.id=s.id AND n.user_id=$1 AND n.read_at IS NULL"#,
    );
    let updated = sqlx::query(&sql)
        .bind(actor.id)
        .bind(selected_ids)
        .bind(MARK_ALL_BATCH)
        .bind(selected_identities)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let unread_count: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM user_notifications n WHERE n.user_id=$1 AND n.read_at IS NULL AND {ELIGIBLE}"
    ))
    .bind(actor.id)
    .fetch_one(&mut *tx)
    .await?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    if let Some(snapshot) = &snapshot {
        ensure_snapshot_live(snapshot)?;
    }
    tx.commit().await?;
    Ok((
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(json!({"updated":updated,"unread_count":unread_count})),
    )
        .into_response())
}

/// Call only after the locked request's decision is updated, inside its commit.
/// The unique event key makes retrying emission harmless without undoing reads.
pub(crate) async fn rank_request_decided(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request_id: i64,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO user_notifications(user_id,event_key,kind,rank_request_id,title,message)
        SELECT r.requester_id,'rank-request:' || r.id || ':' || r.revision,
            'rank_request_' || r.status,r.id,
            left(COALESCE(NULLIF(r.metadata->>'title',''),'Beatmap'),600),
            r.review_reason FROM beatmap_rank_requests r WHERE r.id=$1
            AND r.status IN ('approved','rejected')
        ON CONFLICT(user_id,event_key) DO NOTHING",
    )
    .bind(request_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Only a draft/archive -> published transition emits. Fan-out occurs in the
/// publication transaction and includes existing eligible accounts, never
/// future registrations. Body content is read through the public source route.
pub(crate) async fn announcement_published(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    announcement_id: i64,
    revision: i64,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO user_notifications(user_id,event_key,kind,announcement_id,publication_at,title)
        SELECT u.id,'announcement:' || a.id || ':' || a.revision,
            'announcement_published',a.id,a.published_at,a.title
        FROM announcements a JOIN users u ON u.joined_at<=a.published_at AND NOT u.is_restricted
        WHERE a.id=$1 AND a.revision=$2 AND a.status='published'
        ON CONFLICT(user_id,event_key) DO NOTHING",
    )
    .bind(announcement_id)
    .bind(revision)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    #[test]
    fn notification_reads_require_one_bounded_explicit_selection() {
        for value in [
            json!({"ids":[]}),
            json!({"ids":[0]}),
            json!({"ids":[-1]}),
            json!({"ids":[1,1]}),
            json!({"ids":(1..=101).collect::<Vec<_>>()}),
            json!({"through_id":""}),
            json!({"ids":[1],"through_id":"snapshot"}),
            json!({}),
            json!({"ids":null,"through_id":null}),
        ] {
            let input: Read = serde_json::from_value(value.clone()).unwrap();
            assert!(input.bounds().is_err(), "{value}");
        }
        for value in [json!({"ids":[1,2]}), json!({"through_id":"token"})] {
            let input: Read = serde_json::from_value(value).unwrap();
            assert!(input.bounds().is_ok());
        }
        for value in [
            json!({"ids":[1],"user_id":2}),
            json!({"through_id":"token","all":true}),
            json!({"ids":["1"]}),
            json!({"through_id":u64::MAX}),
        ] {
            assert!(serde_json::from_value::<Read>(value).is_err());
        }
    }

    #[test]
    fn notification_pagination_and_mark_all_work_are_bounded() {
        assert_eq!(
            List {
                page: None,
                page_size: None
            }
            .bounds(),
            (1, 20, 0)
        );
        assert_eq!(
            List {
                page: Some(0),
                page_size: Some(0)
            }
            .bounds(),
            (1, 1, 0)
        );
        assert_eq!(
            List {
                page: Some(u32::MAX),
                page_size: Some(u32::MAX)
            }
            .bounds(),
            (10_000, 50, 499_950)
        );
        assert_eq!(MARK_ALL_BATCH, 1000);
        assert!(serde_json::from_value::<List>(json!({"user_id":2})).is_err());
    }
}
