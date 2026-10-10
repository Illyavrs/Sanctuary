//! A finite server catalogue, independent of official search pagination.
use crate::{
    ApiError, ApiResult, AppState, auth, beatmap_availability as availability, maps, page,
    rank_requests,
};
use axum::{
    Json,
    body::to_bytes,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
};
use illya_domain::official_set::OfficialSetMetadata;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{collections::HashMap, time::Duration};
use tokio::sync::Semaphore;

const MAX_BYTES: usize = 4 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(6);
static SLOTS: Semaphore = Semaphore::const_new(4);
static PUBLIC_SLOTS: Semaphore = Semaphore::const_new(2);

fn changed() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "server_catalogue_changed",
        "The server catalogue changed. Start the search again without a cursor.".into(),
    )
}
fn unsupported() -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, "unsupported_server_search_filter", "This filter is not available in the server catalogue. Clear feature/personal filters and use a supported sort.".into())
}
fn busy() -> ApiError {
    ApiError(
        StatusCode::TOO_MANY_REQUESTS,
        "server_catalogue_busy",
        "The server catalogue is busy. Please retry shortly.".into(),
    )
}
fn native_status(status: &str) -> &str {
    if status == "unranked" {
        "pending"
    } else {
        status
    }
}
fn rank(status: &str) -> i32 {
    match status {
        "graveyard" => -2,
        "wip" => -1,
        "ranked" => 1,
        "approved" => 2,
        "qualified" => 3,
        "loved" => 4,
        _ => 0,
    }
}

#[derive(Clone, Serialize)]
struct Filter {
    q: String,
    mode: Option<i16>,
    statuses: Vec<String>,
    source: Option<String>,
    sort: String,
    uploader: Option<i64>,
    uploaded_only: bool,
    nsfw: bool,
    size: i64,
}
impl Filter {
    fn order(&self) -> ApiResult<&'static str> {
        Ok(match self.sort.as_str() {
            "recent" => "g.added_at DESC,g.native_set_id ASC",
            "updated_desc" | "date_desc" => "g.updated_at DESC,g.native_set_id ASC",
            "updated_asc" | "date_asc" => "g.updated_at ASC,g.native_set_id ASC",
            "ranked_desc" => "g.ranked_at DESC NULLS LAST,g.native_set_id ASC",
            "ranked_asc" => "g.ranked_at ASC NULLS LAST,g.native_set_id ASC",
            "relevance_asc" => "g.relevance ASC,g.native_set_id ASC",
            "relevance_desc" => "g.relevance DESC,g.native_set_id ASC",
            "title" | "title_asc" => "lower(g.title) ASC,g.native_set_id ASC",
            "title_desc" => "lower(g.title) DESC,g.native_set_id ASC",
            "artist_asc" => "lower(g.artist) ASC,g.native_set_id ASC",
            "artist_desc" => "lower(g.artist) DESC,g.native_set_id ASC",
            "stars" | "difficulty_desc" => "g.stars_max DESC,g.native_set_id ASC",
            "difficulty_asc" => "g.stars_min ASC,g.native_set_id ASC",
            "length" => "g.length_max DESC,g.native_set_id ASC",
            _ => return Err(unsupported()),
        })
    }
    fn fingerprint(&self) -> ApiResult<String> {
        let bytes = serde_json::to_vec(self).map_err(ApiError::internal)?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
}
fn text_query(q: String) -> ApiResult<String> {
    if q.len() > 800 || q.chars().count() > 200 || q.chars().any(char::is_control) {
        return Err(ApiError::bad("Supply a bounded catalogue search term."));
    }
    Ok(q.trim().into())
}
fn statuses(value: &str, native: bool) -> ApiResult<Vec<String>> {
    let values: &[&str] = match value {
        "all" | "any" => &[],
        "leaderboard" if native => &["ranked", "loved"],
        "pending" if native => &["pending", "wip", "unranked"],
        "ranked" => &["ranked"],
        "loved" => &["loved"],
        "pending" => &["pending"],
        "graveyard" => &["graveyard"],
        "wip" => &["wip"],
        "unranked" => &["unranked"],
        "qualified" if native => &["qualified"],
        _ => return Err(unsupported()),
    };
    Ok(values.iter().map(|s| (*s).into()).collect())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicFilter {
    q: Option<String>,
    mode: Option<i16>,
    status: Option<String>,
    source: Option<String>,
    sort: Option<String>,
    uploaded_by: Option<i64>,
    uploaded_only: Option<bool>,
    page: Option<u32>,
    page_size: Option<u32>,
}
pub(crate) async fn list(
    State(state): State<AppState>,
    Query(input): Query<PublicFilter>,
) -> ApiResult {
    if input.mode.is_some_and(|m| !(0..=3).contains(&m))
        || input.uploaded_by.is_some_and(|id| id <= 0)
        || input
            .source
            .as_deref()
            .is_some_and(|s| !matches!(s, "official" | "local"))
    {
        return Err(ApiError::bad("Supply supported catalogue filters."));
    }
    let current = i64::from(input.page.unwrap_or(1).max(1));
    let size = i64::from(input.page_size.unwrap_or(24).clamp(1, 50));
    let filter = Filter {
        q: text_query(input.q.unwrap_or_default())?,
        mode: input.mode,
        statuses: statuses(input.status.as_deref().unwrap_or("any"), false)?,
        source: input.source,
        sort: input.sort.unwrap_or_else(|| "recent".into()),
        uploader: input.uploaded_by,
        uploaded_only: input.uploaded_only.unwrap_or(false),
        nsfw: true,
        size,
    };
    filter.order()?;
    let _public = PUBLIC_SLOTS.try_acquire().map_err(|_| busy())?;
    let _slot = SLOTS.try_acquire().map_err(|_| busy())?;
    let (items, total, _) = tokio::time::timeout(
        DEADLINE,
        result(&state, &filter, (current - 1) * size, None),
    )
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_catalogue_timeout",
            "The server catalogue search timed out.".into(),
        )
    })??;
    Ok(page(
        items
            .iter()
            .map(public_item)
            .collect::<ApiResult<Vec<_>>>()?,
        total,
        current,
        size,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeInput {
    user_id: i64,
    query: String,
}
pub(crate) async fn native_search(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
) -> ApiResult {
    maps::require_service_key(&state, &headers)?;
    let body = to_bytes(request.into_body(), 16 * 1024)
        .await
        .map_err(|_| ApiError::bad("The server search body is too large."))?;
    let input: NativeInput = serde_json::from_slice(&body)
        .map_err(|_| ApiError::bad("Supply a user ID and server search query."))?;
    if input.user_id <= 0 {
        return Err(ApiError::bad("Supply a positive user ID."));
    }
    maps::require_active_user(&state, input.user_id).await?;
    let (filter, cursor) = native_filter(&input.query)?;
    let _slot = SLOTS.try_acquire().map_err(|_| busy())?;
    let operation = tokio::time::timeout(DEADLINE, async {
        let (offset, expected) = parse_cursor(cursor.as_deref(), &filter)?;
        let (items, total, epoch) = result(&state, &filter, offset, expected).await?;
        let continuation = if offset + filter.size < total { Some(format!("illya1.{epoch}.{}.{hash}", offset + filter.size, hash=filter.fingerprint()?)) } else { None };
        let sets = items.iter().map(native_item).collect::<ApiResult<Vec<_>>>()?;
        let value = json!({"beatmapsets":sets,"total":total,"catalogue_revision":epoch,
            "cursor":continuation.as_ref().map(|c|json!({"illya":c})),"cursor_string":continuation});
        if serde_json::to_vec(&value).map_err(ApiError::internal)?.len() > MAX_BYTES {
            return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE, "server_catalogue_page_too_large", "Narrow the server search to reduce this page.".into()));
        }
        maps::require_active_user(&state, input.user_id).await?;
        verify_epoch(&state, epoch).await?;
        Ok::<_, ApiError>(value)
    }).await;
    maps::require_service_key(&state, &headers)?;
    maps::require_active_user(&state, input.user_id).await?;
    Ok(Json(operation.map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_catalogue_timeout",
            "The server catalogue search timed out.".into(),
        )
    })??))
}

fn query_encoding_valid(raw: &str) -> bool {
    let mut bytes = raw.bytes();
    let mut decoded = Vec::with_capacity(raw.len());
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let Some(high) = bytes.next().and_then(|b| (b as char).to_digit(16)) else {
                return false;
            };
            let Some(low) = bytes.next().and_then(|b| (b as char).to_digit(16)) else {
                return false;
            };
            decoded.push((high * 16 + low) as u8);
        } else {
            decoded.push(byte);
        }
    }
    std::str::from_utf8(&decoded).is_ok_and(|value| !value.chars().any(char::is_control))
}
fn native_filter(raw: &str) -> ApiResult<(Filter, Option<String>)> {
    if raw.len() > 8192 || !query_encoding_valid(raw) {
        return Err(ApiError::bad(
            "The server search query is too large or malformed.",
        ));
    }
    let url = reqwest::Url::parse(&format!("https://osu.ppy.sh/?{raw}"))
        .map_err(|_| ApiError::bad("Invalid server search query."))?;
    if url.fragment().is_some() {
        return Err(ApiError::bad("Invalid server search fragment."));
    }
    let mut fields = HashMap::new();
    for (key, value) in url.query_pairs() {
        if fields.insert(key.to_string(), value.to_string()).is_some() {
            return Err(ApiError::bad("Duplicate server search field."));
        }
    }
    if fields.len() > 32
        || fields.keys().any(|key| {
            !matches!(
                key.as_str(),
                "q" | "m"
                    | "s"
                    | "sort"
                    | "nsfw"
                    | "g"
                    | "l"
                    | "c"
                    | "e"
                    | "cursor_string"
                    | "cursor[illya]"
                    | "source"
            )
        })
    {
        return Err(unsupported());
    }
    for key in ["g", "l"] {
        if fields.get(key).is_some_and(|v| v != "0") {
            return Err(unsupported());
        }
    }
    for key in ["c", "e"] {
        if fields.get(key).is_some_and(|v| !v.is_empty()) {
            return Err(unsupported());
        }
    }
    let mode = match fields.get("m").map(String::as_str) {
        None | Some("all" | "-1") => None,
        Some(v) => Some(
            v.parse::<i16>()
                .ok()
                .filter(|m| (0..=3).contains(m))
                .ok_or_else(|| ApiError::bad("Invalid server search mode."))?,
        ),
    };
    let nsfw = match fields.get("nsfw").map(String::as_str) {
        None | Some("false") => false,
        Some("true") => true,
        _ => return Err(ApiError::bad("Invalid explicit-content filter.")),
    };
    let uploaded_only = match fields.get("source").map(String::as_str) {
        None => false,
        Some("local") => true,
        _ => return Err(unsupported()),
    };
    let filter = Filter {
        q: text_query(fields.remove("q").unwrap_or_default())?,
        mode,
        statuses: statuses(fields.get("s").map(String::as_str).unwrap_or("any"), true)?,
        source: uploaded_only.then(|| "local".into()),
        sort: fields
            .get("sort")
            .cloned()
            .unwrap_or_else(|| "ranked_desc".into()),
        uploader: None,
        uploaded_only,
        nsfw,
        size: 50,
    };
    filter.order()?;
    let cursor = fields.remove("cursor_string");
    let structured = fields.remove("cursor[illya]");
    if cursor.is_some() && structured.is_some() {
        return Err(ApiError::bad("Supply one server cursor."));
    }
    Ok((filter, cursor.or(structured)))
}
fn parse_cursor(cursor: Option<&str>, filter: &Filter) -> ApiResult<(i64, Option<i64>)> {
    let Some(cursor) = cursor else {
        return Ok((0, None));
    };
    if cursor.len() > 2048 || !cursor.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err(ApiError::bad("Invalid server cursor."));
    }
    let fields = cursor.split('.').collect::<Vec<_>>();
    if fields.len() != 4 || fields[0] != "illya1" || fields[3] != filter.fingerprint()? {
        return Err(changed());
    }
    let epoch = fields[1]
        .parse::<i64>()
        .ok()
        .filter(|v| *v >= 0)
        .ok_or_else(changed)?;
    let offset = fields[2]
        .parse::<i64>()
        .ok()
        .filter(|v| (1..=i64::from(i32::MAX)).contains(v) && *v % filter.size == 0)
        .ok_or_else(changed)?;
    Ok((offset, Some(epoch)))
}
async fn verify_epoch(state: &AppState, expected: i64) -> ApiResult<()> {
    let current: i64 =
        sqlx::query_scalar("SELECT epoch FROM server_catalogue_revision WHERE singleton")
            .fetch_one(&state.db)
            .await?;
    if current != expected {
        return Err(changed());
    }
    Ok(())
}

async fn result(
    state: &AppState,
    filter: &Filter,
    offset: i64,
    expected: Option<i64>,
) -> ApiResult<(Vec<Value>, i64, i64)> {
    let order = filter.order()?;
    let query = format!(
        r#"WITH snapshot_members AS MATERIALIZED (
        SELECT m.native_set_id,(d->>'id')::bigint native_id,b.id local_id,
            m.metadata->>'title' title,m.metadata->>'artist' artist,
            m.metadata->>'creator' creator,d->>'version' version,(d->>'mode_int')::smallint mode,
            COALESCE(o.status,b.status,CASE d->>'status' WHEN 'approved' THEN 'ranked' WHEN 'qualified' THEN 'unranked' ELSE d->>'status' END) status,
            d->>'status' official_status,'official'::text source,(d->>'difficulty_rating')::float8 stars,
            (d->>'total_length')::integer length_seconds,(d->>'bpm')::float8 bpm,(d->>'max_combo')::integer max_combo,
            lower(d->>'checksum') checksum,b.uploaded_by,COALESCE(b.created_at,m.created_at) added_at,
            COALESCE(o.changed_at,(m.metadata->>'ranked_date')::timestamptz,b.created_at,m.verified_at) status_since,
            COALESCE((d->>'last_updated')::timestamptz,b.created_at,m.verified_at) updated_at,m.metadata metadata,d native_meta,
            COALESCE((m.metadata->>'nsfw')::boolean,false) nsfw,
            b.id IS NOT NULL AND (lower(b.md5)<>lower(d->>'checksum') OR b.mode<>(d->>'mode_int')::smallint
                OR b.source<>'official' OR b.official_id IS DISTINCT FROM (d->>'id')::bigint
                OR b.official_set_id IS DISTINCT FROM m.native_set_id) version_conflict,
            EXISTS(SELECT 1 FROM official_beatmapset_archive_members am JOIN official_beatmapset_archives a ON a.id=am.archive_id
                WHERE am.beatmap_id=b.id AND a.official_set_id=m.native_set_id
                    AND a.member_md5=ARRAY(SELECT lower(member->>'checksum') FROM jsonb_array_elements(m.metadata->'beatmaps') member ORDER BY lower(member->>'checksum'))) archive_available
        FROM beatmap_catalogue_metadata m CROSS JOIN LATERAL jsonb_array_elements(m.metadata->'beatmaps') d
        LEFT JOIN beatmaps b ON b.native_id=(d->>'id')::bigint AND b.native_set_id=m.native_set_id
        LEFT JOIN beatmap_status_overrides o ON o.native_id=(d->>'id')::bigint AND o.native_set_id=m.native_set_id
        WHERE illya_beatmapset_active(m.native_set_id) AND
            (EXISTS(SELECT 1 FROM beatmap_status_overrides so WHERE so.native_set_id=m.native_set_id)
             OR EXISTS(SELECT 1 FROM beatmaps stored WHERE stored.native_set_id=m.native_set_id))
    ), stored_members AS MATERIALIZED (
        SELECT b.native_set_id,b.native_id,b.id local_id,b.title,b.artist,b.creator,b.version,b.mode,
            COALESCE(o.status,b.status) status,NULL::text official_status,b.source,b.stars,b.length_seconds,b.bpm,b.max_combo,
            lower(b.md5) checksum,b.uploaded_by,b.created_at added_at,COALESCE(o.changed_at,b.created_at) status_since,
            b.created_at updated_at,NULL::jsonb metadata,NULL::jsonb native_meta,false nsfw,false version_conflict,
            (b.archive_path IS NOT NULL OR EXISTS(SELECT 1 FROM beatmap_packages p WHERE p.id=b.local_package_id AND p.kind='osz')
             OR EXISTS(SELECT 1 FROM official_beatmapset_archive_members am WHERE am.beatmap_id=b.id)) archive_available
        FROM active_beatmaps b LEFT JOIN beatmap_status_overrides o ON o.native_id=b.native_id AND o.native_set_id=b.native_set_id
        WHERE NOT EXISTS(SELECT 1 FROM snapshot_members sm WHERE sm.native_set_id=b.native_set_id AND sm.native_id=b.native_id)
    ), members AS MATERIALIZED (SELECT * FROM snapshot_members UNION ALL SELECT * FROM stored_members), matching AS MATERIALIZED (
        SELECT *, CASE WHEN $1='' THEN 0 ELSE
            CASE WHEN lower(title)=lower($10) THEN 8 WHEN strpos(lower(title),lower($10))>0 THEN 4 ELSE 0 END
            + CASE WHEN lower(artist)=lower($10) THEN 4 WHEN strpos(lower(artist),lower($10))>0 THEN 2 ELSE 0 END
            + CASE WHEN strpos(lower(version),lower($10))>0 THEN 1 ELSE 0 END END relevance
        FROM members WHERE concat_ws(' ',title,artist,creator,version,metadata->>'tags') ILIKE '%'||$1||'%'
            AND ($2::smallint IS NULL OR mode=$2) AND (cardinality($3::text[])=0 OR status=ANY($3))
            AND ($4::text IS NULL OR source=$4) AND ($5::bigint IS NULL OR uploaded_by=$5)
            AND (NOT $6::boolean OR (source='local' AND uploaded_by IS NOT NULL AND native_set_id>=2000000000))
            AND ($7::boolean OR NOT nsfw)
    ), grouped AS (
        SELECT native_set_id,min(local_id) representative_id,min(title) title,min(artist) artist,min(creator) creator,min(source) source,
            count(*) matching_count,array_agg(DISTINCT mode ORDER BY mode) matching_modes,
            array_agg(DISTINCT status ORDER BY status) matching_statuses,min(stars) stars_min,max(stars) stars_max,
            min(length_seconds) length_min,max(length_seconds) length_max,max(added_at) added_at,max(updated_at) updated_at,max(relevance) relevance,
            max(status_since) FILTER(WHERE status IN('ranked','loved')) ranked_at
        FROM matching GROUP BY native_set_id
    ), selected AS (SELECT g.*,row_number() OVER(ORDER BY {order}) item_order FROM grouped g ORDER BY {order} LIMIT $8 OFFSET $9), projected AS (
        SELECT g.item_order,jsonb_build_object('native_set_id',g.native_set_id,'title',g.title,'artist',g.artist,'creator',g.creator,'source',g.source,
            'metadata',(SELECT m.metadata FROM beatmap_catalogue_metadata m WHERE m.native_set_id=g.native_set_id),
            'members',(SELECT jsonb_agg(to_jsonb(mm)-'metadata' ORDER BY mm.mode,mm.stars,mm.native_id) FROM members mm WHERE mm.native_set_id=g.native_set_id),
            'stored',jsonb_build_object('native_set_id',g.native_set_id,
                'representative',(SELECT {public_map}||jsonb_build_object('availability',{public_state}) FROM beatmaps b
                    LEFT JOIN beatmapset_availability av ON av.native_set_id=b.native_set_id WHERE b.id=g.representative_id),
                'matching_count',g.matching_count,'stored_count',(SELECT count(*) FROM beatmaps stored WHERE stored.native_set_id=g.native_set_id),
                'matching_modes',g.matching_modes,'matching_statuses',g.matching_statuses,'stars_min',g.stars_min,'stars_max',g.stars_max,
                'length_min',g.length_min,'length_max',g.length_max,'added_at',g.added_at,
                'preview',(SELECT COALESCE(jsonb_agg(jsonb_build_object('id',p.local_id,'native_id',p.native_id,'title',p.title,'artist',p.artist,'creator',p.creator,
                    'version',p.version,'mode',p.mode,'status',p.status,'source',p.source,'stars',p.stars,'length_seconds',p.length_seconds,'bpm',p.bpm,'max_combo',p.max_combo)
                    ORDER BY p.mode,p.stars,p.native_id),'[]'::jsonb) FROM members p WHERE p.native_set_id=g.native_set_id)
            )) item FROM selected g
    ) SELECT COALESCE((SELECT jsonb_agg(item ORDER BY item_order) FROM projected),'[]'::jsonb) items,
        (SELECT count(*) FROM grouped) total,(SELECT epoch FROM server_catalogue_revision WHERE singleton) epoch"#,
        public_map = maps::PUBLIC_MAP,
        public_state = availability::PUBLIC_STATE
    );
    let escaped = filter
        .q
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let row = sqlx::query(&query)
        .bind(escaped)
        .bind(filter.mode)
        .bind(&filter.statuses)
        .bind(&filter.source)
        .bind(filter.uploader)
        .bind(filter.uploaded_only)
        .bind(filter.nsfw)
        .bind(filter.size)
        .bind(offset)
        .bind(&filter.q)
        .fetch_one(&state.db)
        .await?;
    let epoch: i64 = row.try_get("epoch")?;
    if expected.is_some_and(|v| v != epoch) {
        return Err(changed());
    }
    let total: i64 = row.try_get("total")?;
    if total > i64::from(i32::MAX) {
        return Err(ApiError::internal(
            "catalogue count exceeds the native contract",
        ));
    }
    let value: Value = row.try_get("items")?;
    if serde_json::to_vec(&value)
        .map_err(ApiError::internal)?
        .len()
        > MAX_BYTES
    {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "server_catalogue_page_too_large",
            "Narrow the search or use a smaller page.".into(),
        ));
    }
    let items = value
        .as_array()
        .cloned()
        .ok_or_else(|| ApiError::internal("invalid server catalogue projection"))?;
    if items.iter().any(|i| {
        i["members"]
            .as_array()
            .is_some_and(|ms| ms.iter().any(|m| m["version_conflict"] == true))
    }) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "official_version_conflict",
            "The stored content and published catalogue metadata have different checksums.".into(),
        ));
    }
    verify_epoch(state, epoch).await?;
    Ok((items, total, epoch))
}

fn members(item: &Value) -> ApiResult<&Vec<Value>> {
    item["members"]
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= 512)
        .ok_or_else(|| ApiError::internal("invalid catalogue membership"))
}
fn summary(item: &Value) -> ApiResult<String> {
    let maps = members(item)?;
    let first = maps[0]["status"]
        .as_str()
        .ok_or_else(|| ApiError::internal("invalid catalogue status"))?;
    Ok(if maps.iter().all(|m| m["status"] == first) {
        first
    } else {
        "pending"
    }
    .into())
}
fn public_item(item: &Value) -> ApiResult<Value> {
    let metadata = &item["metadata"];
    if metadata.is_null() {
        return Ok(json!({"kind":"stored","stored":item["stored"]}));
    }
    let maps = members(item)?;
    let stored = maps.iter().filter(|m| m["local_id"].is_i64()).count();
    let archive = maps.iter().any(|m| m["archive_available"] == true);
    let difficulties=maps.iter().map(|m| {
        let mut value=json!({"official_id":m["native_id"],"status":m["status"],"official_status":m["official_status"],"version":m["version"],
            "mode":m["mode"],"stars":m["stars"],"length_seconds":m["length_seconds"],"checksum":m["checksum"],"bpm":m["bpm"],
            "max_combo":m["max_combo"],"archive_available":m["archive_available"],"ar":m["native_meta"]["ar"],"cs":m["native_meta"]["cs"],
            "od":m["native_meta"]["accuracy"],"hp":m["native_meta"]["drain"]});
        if m["local_id"].is_i64() { value["local_id"]=m["local_id"].clone(); }
        value
    }).collect::<Vec<_>>();
    let set = item["native_set_id"]
        .as_i64()
        .ok_or_else(|| ApiError::internal("invalid catalogue identity"))?;
    Ok(
        json!({"kind":"official_metadata","official":{"official_set_id":set,"title":item["title"],"artist":item["artist"],"creator":item["creator"],
        "status":summary(item)?,"official_status":metadata["status"],"metadata_only":stored==0,
        "cover_url":format!("https://assets.ppy.sh/beatmaps/{set}/covers/cover.jpg"),"official_url":format!("https://osu.ppy.sh/beatmapsets/{set}"),
        "download_disabled":metadata["download_disabled"]==true||!archive,"difficulties":difficulties,
        "local":{"stored_count":stored,"archive_available":archive,"withdrawn":false}}}),
    )
}
fn native_item(item: &Value) -> ApiResult<Value> {
    let maps = members(item)?;
    let status = summary(item)?;
    let stored = maps.iter().filter(|m| m["local_id"].is_i64()).count();
    let archive = maps.iter().any(|m| m["archive_available"] == true);
    let mut set = if item["metadata"].is_object() {
        item["metadata"].clone()
    } else {
        json!({"id":item["native_set_id"],"title":item["title"],"artist":item["artist"],"creator":item["creator"],"user_id":0,"covers":{},"source":"","tags":""})
    };
    let beatmaps=maps.iter().map(|m| {
        let status=m["status"].as_str().unwrap_or("pending");
        let mut value=if m["native_meta"].is_object() { m["native_meta"].clone() } else {
            let mode=m["mode"].as_u64().unwrap_or(0) as usize;
            json!({"id":m["native_id"],"beatmapset_id":m["native_set_id"],"version":m["version"],"mode_int":mode,
                "mode":(["osu","taiko","fruits","mania"][mode]),"difficulty_rating":m["stars"],"total_length":m["length_seconds"],
                "bpm":m["bpm"],"max_combo":m["max_combo"],"convert":false,"user_id":0,"owners":[],"is_scoreable":true})
        };
        value["checksum"]=m["checksum"].clone();value["status"]=json!(native_status(status));value["ranked"]=json!(rank(native_status(status)));
        value
    }).collect::<Vec<_>>();
    let disabled = set["download_disabled"] == true || !archive;
    set["status"] = json!(native_status(&status));
    set["ranked"] = json!(rank(native_status(&status)));
    set["beatmaps"] = json!(beatmaps);
    set["metadata_only"] = json!(stored == 0);
    set["download_disabled"] = json!(disabled);
    set["availability"] = json!({"download_disabled":disabled,"more_information":null});
    Ok(set)
}

/// Caller holds the exclusive active set fence and fresh mutation authority.
pub(crate) async fn publish(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    set_id: i64,
    snapshot: &Value,
    reauthorized: &[i64],
) -> ApiResult<()> {
    OfficialSetMetadata::from_upstream(snapshot, set_id)
        .map_err(|_| ApiError::internal("invalid validated catalogue metadata"))?;
    if snapshot["download_disabled"].as_bool().is_none()
        || serde_json::to_vec(snapshot)
            .map_err(ApiError::internal)?
            .len()
            > MAX_BYTES
    {
        return Err(ApiError::internal(
            "invalid catalogue metadata size or availability",
        ));
    }
    let previous: Option<Value> = sqlx::query_scalar(
        "SELECT metadata FROM beatmap_catalogue_metadata WHERE native_set_id=$1 FOR UPDATE",
    )
    .bind(set_id)
    .fetch_optional(&mut **tx)
    .await?;
    let current = snapshot["beatmaps"]
        .as_array()
        .ok_or_else(|| ApiError::internal("invalid catalogue manifest"))?;
    let archive: Option<Vec<String>> = sqlx::query_scalar(
        "SELECT member_md5 FROM official_beatmapset_archives WHERE official_set_id=$1",
    )
    .bind(set_id)
    .fetch_optional(&mut **tx)
    .await?;
    let mut complete = current
        .iter()
        .map(|m| {
            m["checksum"]
                .as_str()
                .map(str::to_ascii_lowercase)
                .ok_or_else(changed)
        })
        .collect::<ApiResult<Vec<_>>>()?;
    complete.sort();
    if archive.is_some_and(|manifest| manifest != complete) {
        return Err(changed());
    }
    // Older nominations already retain an immutable checksum proof. Use it for
    // repair without ever publishing private nomination data in the catalogue.
    let proofs=sqlx::query("SELECT DISTINCT ON ((member->>'native_id')::bigint) (member->>'native_id')::bigint native_id,member->>'md5' md5 FROM beatmap_rank_requests r CROSS JOIN LATERAL jsonb_array_elements(r.metadata->'manifest') member WHERE r.native_set_id=$1 AND r.status='approved' AND (member->>'native_id')::bigint=ANY(r.approved_difficulty_ids) ORDER BY (member->>'native_id')::bigint,r.reviewed_at DESC,r.id DESC")
        .bind(set_id).fetch_all(&mut **tx).await?;
    let rows=sqlx::query("SELECT candidate.native_id,b.md5,b.mode,b.source,b.official_id,b.official_set_id,o.native_id IS NOT NULL overridden FROM (SELECT native_id FROM beatmap_status_overrides WHERE native_set_id=$1 UNION SELECT native_id FROM beatmaps WHERE native_set_id=$1) candidate LEFT JOIN beatmaps b ON b.native_id=candidate.native_id AND b.native_set_id=$1 LEFT JOIN beatmap_status_overrides o ON o.native_id=candidate.native_id AND o.native_set_id=$1")
        .bind(set_id).fetch_all(&mut **tx).await?;
    for row in rows {
        let id: i64 = row.get("native_id");
        let map = current
            .iter()
            .find(|m| m["id"].as_i64() == Some(id))
            .ok_or_else(changed)?;
        let checksum = map["checksum"].as_str().ok_or_else(changed)?;
        let stored: Option<String> = row.get("md5");
        if stored.is_some_and(|s| !s.eq_ignore_ascii_case(checksum)) {
            return Err(changed());
        }
        if row.get::<Option<String>, _>("md5").is_some()
            && (row.get::<Option<i16>, _>("mode").map(i64::from) != map["mode_int"].as_i64()
                || row.get::<Option<String>, _>("source").as_deref() != Some("official")
                || row.get::<Option<i64>, _>("official_id") != Some(id)
                || row.get::<Option<i64>, _>("official_set_id") != Some(set_id))
        {
            return Err(changed());
        }
        if row.get::<bool, _>("overridden") && !reauthorized.contains(&id) {
            let prior = previous
                .as_ref()
                .and_then(|p| p["beatmaps"].as_array())
                .and_then(|ms| ms.iter().find(|m| m["id"].as_i64() == Some(id)))
                .and_then(|m| m["checksum"].as_str());
            let proof = proofs
                .iter()
                .find(|r| r.get::<i64, _>("native_id") == id)
                .map(|r| r.get::<String, _>("md5"));
            let exact = prior.or(proof.as_deref());
            if exact.is_some_and(|s| !s.eq_ignore_ascii_case(checksum))
                || (exact.is_none() && row.get::<Option<String>, _>("md5").is_none())
            {
                return Err(changed());
            }
        }
    }
    sqlx::query("INSERT INTO beatmap_catalogue_metadata(native_set_id,metadata) VALUES($1,$2) ON CONFLICT(native_set_id) DO UPDATE SET metadata=EXCLUDED.metadata")
        .bind(set_id).bind(snapshot).execute(&mut **tx).await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Refresh {
    reason: String,
    reaffirm_difficulties: Option<Vec<Reaffirm>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reaffirm {
    native_id: i64,
    checksum: String,
    expected_revision: i64,
    #[serde(deserialize_with = "required_nullable_revision")]
    expected_status_revision: Option<i64>,
}
fn required_nullable_revision<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<i64>, D::Error> {
    Option::<i64>::deserialize(deserializer)
}
pub(crate) async fn refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(set_id): Path<i64>,
    Json(input): Json<Refresh>,
) -> ApiResult {
    let user = auth::require(&state, &headers).await?;
    user.require("beatmaps.moderate")?;
    let reason = input.reason.trim();
    if reason.is_empty() || reason.chars().count() > 1000 || reason.chars().any(char::is_control) {
        return Err(ApiError::bad("Supply a bounded refresh reason."));
    }
    if !(1..=illya_domain::official_set::MAX_OFFICIAL_ID).contains(&set_id) {
        return Err(ApiError::bad("Supply an official beatmap set ID."));
    }
    let mut reaffirm = input.reaffirm_difficulties.unwrap_or_default();
    if reaffirm.len() > 512 {
        return Err(ApiError::bad(
            "Supply at most 512 explicit difficulty reaffirmations.",
        ));
    }
    reaffirm.sort_by_key(|item| item.native_id);
    if reaffirm
        .windows(2)
        .any(|pair| pair[0].native_id == pair[1].native_id)
        || reaffirm.iter().any(|item| {
            !(1..=illya_domain::official_set::MAX_OFFICIAL_ID).contains(&item.native_id)
                || item.checksum.len() != 32
                || !item.checksum.bytes().all(|b| b.is_ascii_hexdigit())
                || item.expected_revision < 0
                || item.expected_status_revision.is_some_and(|r| r < 0)
        })
    {
        return Err(ApiError::bad(
            "Supply distinct canonical difficulties, checksums and current revisions.",
        ));
    }
    let snapshot = maps::catalogue_snapshot(&state, set_id).await;
    auth::require(&state, &headers)
        .await?
        .require("beatmaps.moderate")?;
    let snapshot = snapshot?;
    let mut tx = state.db.begin().await?;
    let actor = auth::mutation_actor(&mut tx, &user, &[], auth::MutationScope::Permissions).await?;
    actor.require("beatmaps.moderate")?;
    availability::lock_exclusive_set(&mut tx, set_id).await?;
    availability::lock_active_set(&mut tx, set_id).await?;
    let known:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM beatmap_status_overrides WHERE native_set_id=$1) OR EXISTS(SELECT 1 FROM beatmaps WHERE native_set_id=$1)").bind(set_id).fetch_one(&mut *tx).await?;
    if !known {
        return Err(ApiError::missing());
    }
    let mut reauthorized = Vec::with_capacity(reaffirm.len());
    for item in &reaffirm {
        let map = snapshot["beatmaps"]
            .as_array()
            .and_then(|maps| {
                maps.iter()
                    .find(|map| map["id"].as_i64() == Some(item.native_id))
            })
            .ok_or_else(changed)?;
        if map["checksum"]
            .as_str()
            .is_none_or(|checksum| !checksum.eq_ignore_ascii_case(&item.checksum))
        {
            return Err(changed());
        }
        let stored = rank_requests::locked_difficulty(&mut tx, set_id, item.native_id).await?;
        rank_requests::check_difficulty_revision(
            &mut tx,
            set_id,
            &rank_requests::DifficultyRevision::new(
                item.native_id,
                item.expected_revision,
                item.expected_status_revision,
            ),
            stored.as_ref(),
        )
        .await?;
        let status:Option<String>=sqlx::query_scalar("SELECT status FROM beatmap_status_overrides WHERE native_set_id=$1 AND native_id=$2 FOR UPDATE")
            .bind(set_id).bind(item.native_id).fetch_optional(&mut *tx).await?;
        let status = status.ok_or_else(changed)?;
        rank_requests::set_difficulty_status(
            &mut tx,
            &actor,
            set_id,
            item.native_id,
            &status,
            reason,
            stored.as_ref(),
        )
        .await?;
        reauthorized.push(item.native_id);
    }
    publish(&mut tx, set_id, &snapshot, &reauthorized).await?;
    sqlx::query("INSERT INTO audit_events(actor_id,action,target_type,target_id,details) VALUES($1,'beatmaps.catalogue_metadata','beatmapset',$2,$3)")
        .bind(actor.id).bind(set_id.to_string()).bind(json!({"reason":reason,"reaffirmed_difficulty_ids":reauthorized})).execute(&mut *tx).await?;
    auth::ensure_mutation_actor(&mut tx, &actor).await?;
    tx.commit().await?;
    Ok(Json(json!({"native_set_id":set_id,"refreshed":true})))
}
pub(crate) async fn pending(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(pagination): Query<illya_domain::Pagination>,
) -> ApiResult {
    auth::require(&state, &headers)
        .await?
        .require("beatmaps.moderate")?;
    let (current, size, _) = pagination.bounds();
    let size = size.min(50);
    let offset = (current - 1) * size;
    let row=sqlx::query("WITH missing AS MATERIALIZED (SELECT o.native_set_id,count(*) override_count FROM beatmap_status_overrides o WHERE o.native_set_id<2000000000 AND illya_beatmapset_active(o.native_set_id) AND NOT EXISTS(SELECT 1 FROM beatmap_catalogue_metadata m WHERE m.native_set_id=o.native_set_id) GROUP BY o.native_set_id), selected AS (SELECT * FROM missing ORDER BY native_set_id LIMIT $1 OFFSET $2) SELECT COALESCE((SELECT jsonb_agg(to_jsonb(s) ORDER BY native_set_id) FROM selected s),'[]'::jsonb) items,(SELECT count(*) FROM missing) total")
        .bind(size).bind(offset).fetch_one(&state.db).await?;
    auth::require(&state, &headers)
        .await?
        .require("beatmaps.moderate")?;
    let items: Value = row.get("items");
    Ok(page(
        items
            .as_array()
            .cloned()
            .ok_or_else(|| ApiError::internal("invalid pending catalogue projection"))?,
        row.get("total"),
        current,
        size,
    ))
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    #[test]
    fn native_defaults_and_unsupported_fields_are_explicit() {
        let (f, c) = native_filter("q=%E6%98%9F&s=ranked&sort=ranked_desc&nsfw=false").unwrap();
        assert_eq!(f.q, "星");
        assert_eq!(f.statuses, vec!["ranked"]);
        assert!(c.is_none());
        for q in [
            "c=featured_artists",
            "e=video",
            "g=2",
            "l=3",
            "played=played",
            "s=mine",
            "sort=plays_desc",
        ] {
            assert!(native_filter(q).is_err(), "{q}");
        }
    }
    #[test]
    fn explicit_local_native_search_is_uploaded_only_and_cursor_bound() {
        let (broad, _) = native_filter("s=ranked").unwrap();
        let (uploaded, _) = native_filter("source=local&s=ranked").unwrap();
        assert_eq!(broad.source, None);
        assert!(!broad.uploaded_only);
        assert_eq!(uploaded.source.as_deref(), Some("local"));
        assert!(uploaded.uploaded_only);
        let token = format!("illya1.3.50.{}", broad.fingerprint().unwrap());
        assert!(parse_cursor(Some(&token), &uploaded).is_err());
        assert!(native_filter("source=official").is_err());
    }
    #[test]
    fn cursor_is_query_bound_and_roundtrips_both_stock_and_website_forms() {
        let (f, _) = native_filter("s=ranked").unwrap();
        let token = format!("illya1.3.50.{}", f.fingerprint().unwrap());
        assert_eq!(parse_cursor(Some(&token), &f).unwrap(), (50, Some(3)));
        let deep = format!("illya1.3.1000050.{}", f.fingerprint().unwrap());
        assert_eq!(parse_cursor(Some(&deep), &f).unwrap(), (1_000_050, Some(3)));
        let (other, _) = native_filter("s=loved").unwrap();
        assert!(parse_cursor(Some(&token), &other).is_err());
        assert!(native_filter("cursor[illya]=a&cursor_string=a").is_err());
    }
}
