//! Official discovery shares bounded upstream caches for players and staff.
//! Public browsing never imports maps. Local availability is merged afresh.
use super::*;
use axum::{
    body::to_bytes,
    extract::{RawQuery, Request},
};
use serde::Serialize;

const MAX_ENTRIES: usize = 16;
const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;
const POSITIVE_TTL: Duration = Duration::from_secs(60);
const OPERATION_DEADLINE: Duration = Duration::from_secs(6);
const MAX_NATIVE_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct SearchKey {
    q: String,
    mode: String,
    status: String,
    sort: String,
    cursor: Option<String>,
    extras: Vec<(String, String)>,
    structured_cursor: Vec<(String, String)>,
    set_id: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    #[serde(default)]
    q: String,
    #[serde(default = "all")]
    mode: String,
    #[serde(default = "ranked")]
    status: String,
    #[serde(default = "ranked_desc")]
    sort: String,
    cursor_string: Option<String>,
}
fn all() -> String {
    "all".into()
}
fn ranked() -> String {
    "ranked".into()
}
fn ranked_desc() -> String {
    "ranked_desc".into()
}

fn cursor_valid(value: &str) -> bool {
    !value.is_empty() && value.len() <= 2048 && value.bytes().all(|c| (0x20..=0x7e).contains(&c))
}

fn query_encoding_valid(raw: &str) -> bool {
    let mut decoded = Vec::with_capacity(raw.len());
    let mut bytes = raw.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let Some(high) = bytes.next().and_then(|byte| (byte as char).to_digit(16)) else {
                return false;
            };
            let Some(low) = bytes.next().and_then(|byte| (byte as char).to_digit(16)) else {
                return false;
            };
            decoded.push((high * 16 + low) as u8);
        } else {
            decoded.push(byte);
        }
    }
    std::str::from_utf8(&decoded).is_ok()
}

fn parse(raw: Option<String>) -> ApiResult<SearchKey> {
    let raw = raw.unwrap_or_default();
    if raw.len() > 8192 {
        return Err(ApiError::bad("The search query exceeds its size limit."));
    }
    if !query_encoding_valid(&raw) {
        return Err(ApiError::bad("Invalid search query encoding."));
    }
    let uri = format!("/?{raw}")
        .parse()
        .map_err(|_| ApiError::bad("Invalid search query."))?;
    let Query(input) = Query::<Input>::try_from_uri(&uri)
        .map_err(|_| ApiError::bad("Supply supported official search fields."))?;
    if input.q.len() > 800
        || input.q.chars().count() > 200
        || input.q.chars().any(char::is_control)
        || !matches!(input.mode.as_str(), "all" | "0" | "1" | "2" | "3")
        || !matches!(
            input.status.as_str(),
            "any" | "ranked" | "qualified" | "loved" | "pending" | "graveyard"
        )
        || !matches!(
            input.sort.as_str(),
            "ranked_desc" | "date_desc" | "title_asc" | "difficulty_desc"
        )
        || input
            .cursor_string
            .as_deref()
            .is_some_and(|cursor| !cursor_valid(cursor))
    {
        return Err(ApiError::bad(
            "Supply a bounded search term, supported filters and cursor.",
        ));
    }
    Ok(SearchKey {
        q: input.q.trim().into(),
        mode: input.mode,
        status: input.status,
        sort: input.sort,
        cursor: input.cursor_string,
        extras: Vec::new(),
        structured_cursor: Vec::new(),
        set_id: None,
    })
}

fn unsupported_filter() -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        "unsupported_search_filter",
        "Personal official search filters are unavailable for Illyaverse accounts.".into(),
    )
}

fn native_parse(raw: &str) -> ApiResult<SearchKey> {
    if raw.len() > 8192 || !query_encoding_valid(raw) || raw.chars().any(char::is_control) {
        return Err(ApiError::bad("Invalid or oversized native search query."));
    }
    let url = reqwest::Url::parse(&format!("https://osu.ppy.sh/?{raw}"))
        .map_err(|_| ApiError::bad("Invalid native search query."))?;
    if url.fragment().is_some() {
        return Err(ApiError::bad("Invalid native search query fragment."));
    }
    let mut fields = HashMap::new();
    let mut structured_cursor = Vec::new();
    for (name, value) in url.query_pairs() {
        if fields.insert(name.to_string(), value.to_string()).is_some() {
            return Err(ApiError::bad("Duplicate native search query fields."));
        }
        if let Some(name) = name
            .strip_prefix("cursor[")
            .and_then(|name| name.strip_suffix(']'))
        {
            if !cursor_field(name)
                || !cursor_query_part(name, &value)
                || structured_cursor.len() >= 16
            {
                return Err(ApiError::bad("Invalid native search cursor."));
            }
            structured_cursor.push((name.to_owned(), value.into_owned()));
        } else if !matches!(
            name.as_ref(),
            "q" | "m"
                | "s"
                | "sort"
                | "c"
                | "g"
                | "l"
                | "e"
                | "r"
                | "played"
                | "nsfw"
                | "cursor_string"
        ) {
            return Err(ApiError::bad("Unsupported native search query field."));
        }
    }
    let q = fields.get("q").map(String::as_str).unwrap_or_default();
    let mode = fields.get("m").map(String::as_str).unwrap_or("all");
    let status = fields.get("s").map(String::as_str).unwrap_or("any");
    let sort = fields
        .get("sort")
        .map(String::as_str)
        .unwrap_or("ranked_desc");
    if q.len() > 800
        || q.chars().count() > 200
        || q.chars().any(char::is_control)
        || !matches!(mode, "all" | "0" | "1" | "2" | "3")
        || !matches!(
            status,
            "any"
                | "leaderboard"
                | "ranked"
                | "qualified"
                | "loved"
                | "pending"
                | "wip"
                | "graveyard"
                | "favourites"
                | "mine"
        )
        || !sort.split_once('_').is_some_and(|(field, order)| {
            matches!(
                field,
                "title"
                    | "artist"
                    | "difficulty"
                    | "updated"
                    | "ranked"
                    | "rating"
                    | "plays"
                    | "favourites"
                    | "relevance"
                    | "nominations"
            ) && matches!(order, "asc" | "desc")
        })
    {
        return Err(ApiError::bad("Supply supported native search filters."));
    }
    if matches!(status, "favourites" | "mine")
        || fields.contains_key("played")
        || fields.contains_key("r")
    {
        return Err(unsupported_filter());
    }
    let mut extras = Vec::new();
    if let Some(value) = fields.get("nsfw") {
        if !matches!(value.as_str(), "true" | "false") {
            return Err(ApiError::bad("Invalid NSFW filter."));
        }
        if value == "true" {
            extras.push(("nsfw".into(), value.clone()));
        }
    }
    for name in ["g", "l"] {
        if let Some(value) = fields.get(name) {
            let number = value
                .parse::<i32>()
                .map_err(|_| ApiError::bad("Invalid genre or language filter."))?;
            if number.to_string() != *value
                || !(0..=14).contains(&number)
                || (name == "g" && number == 8)
            {
                return Err(ApiError::bad("Invalid genre or language filter."));
            }
            if number != 0 {
                extras.push((name.into(), value.clone()));
            }
        }
    }
    for name in ["c", "e"] {
        if let Some(value) = fields.get(name).filter(|value| !value.is_empty()) {
            let mut tokens = value.split('.').collect::<Vec<_>>();
            if tokens.len() > 5
                || tokens.iter().any(|token| {
                    if name == "c" {
                        !matches!(
                            *token,
                            "recommended"
                                | "converts"
                                | "follows"
                                | "spotlights"
                                | "featured_artists"
                        )
                    } else {
                        !matches!(*token, "video" | "storyboard")
                    }
                })
            {
                return Err(ApiError::bad("Invalid native search feature filter."));
            }
            if tokens
                .iter()
                .any(|token| matches!(*token, "recommended" | "follows"))
            {
                return Err(unsupported_filter());
            }
            tokens.sort_unstable();
            let count = tokens.len();
            tokens.dedup();
            if tokens.len() != count {
                return Err(ApiError::bad("Duplicate native search feature filters."));
            }
            extras.push((name.into(), tokens.join(".")));
        }
    }
    let cursor = fields.get("cursor_string").cloned();
    if cursor
        .as_deref()
        .is_some_and(|cursor| !cursor_valid(cursor))
        || (cursor.is_some() && !structured_cursor.is_empty())
    {
        return Err(ApiError::bad("Supply one supported native search cursor."));
    }
    if !structured_cursor.is_empty() {
        let has = |field: &str| structured_cursor.iter().any(|(name, _)| name == field);
        if required_cursor_fields(sort, status)
            .iter()
            .any(|field| !has(field))
        {
            return Err(ApiError::bad(
                "The native search cursor is incomplete for this sort.",
            ));
        }
    }
    extras.sort();
    structured_cursor.sort();
    Ok(SearchKey {
        q: q.trim().into(),
        mode: mode.into(),
        status: status.into(),
        sort: if sort == "updated_desc" {
            "date_desc".into()
        } else {
            sort.into()
        },
        cursor,
        extras,
        structured_cursor,
        set_id: None,
    })
}

fn required_cursor_fields(sort: &str, status: &str) -> Vec<&'static str> {
    let field = match sort.split_once('_').map(|(field, _)| field) {
        Some("artist") => "artist.raw",
        Some("difficulty") => "beatmaps.difficultyrating",
        Some("favourites") => "favourite_count",
        Some("nominations") => "nominations",
        Some("plays") => "play_count",
        Some("ranked") => "approved_date",
        Some("rating") => "rating",
        Some("relevance") => "_score",
        Some("title") => "title.raw",
        Some("updated" | "date") => "last_update",
        _ => unreachable!("validated sort"),
    };
    let mut fields = vec!["id", field];
    if field == "nominations" {
        fields.push("hype");
    }
    if field == "approved_date" && status == "qualified" {
        fields.push("queued_at");
    }
    fields
}
fn cursor_complete(result: &SearchResult, key: &SearchKey) -> ApiResult<()> {
    if let Some(cursor) = &result.native_cursor
        && required_cursor_fields(&key.sort, &key.status)
            .iter()
            .any(|field| cursor.get(*field).is_none())
    {
        return Err(invalid());
    }
    Ok(())
}

fn cursor_field(name: &str) -> bool {
    matches!(
        name,
        "artist.raw"
            | "creator.raw"
            | "title.raw"
            | "beatmaps.difficultyrating"
            | "rating"
            | "favourite_count"
            | "play_count"
            | "nominations"
            | "hype"
            | "id"
            | "approved_date"
            | "last_update"
            | "queued_at"
            | "_score"
    )
}
fn cursor_query_part(name: &str, value: &str) -> bool {
    if !cursor_part(value) {
        return false;
    }
    match name {
        "artist.raw" | "creator.raw" | "title.raw" => true,
        "beatmaps.difficultyrating" | "rating" | "_score" => value
            .parse::<f64>()
            .is_ok_and(|number| number.is_finite() && (0.0..=1e12).contains(&number)),
        "id" => value
            .parse::<i64>()
            .is_ok_and(|id| (1..=MAX_OFFICIAL_ID).contains(&id)),
        "approved_date" | "last_update" | "queued_at" => {
            value.parse::<i64>().is_ok_and(|number| number >= 0)
                || (value.len() <= 64
                    && chrono::DateTime::parse_from_rfc3339(value).is_ok_and(|date| {
                        date.timestamp() >= 0 && chrono::Datelike::year(&date) <= 9999
                    }))
        }
        _ => value.parse::<i64>().is_ok_and(|number| number >= 0),
    }
}
fn cursor_part(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
}

impl SearchKey {
    fn path(&self) -> String {
        if let Some(id) = self.set_id {
            return format!("/api/v2/beatmapsets/{id}");
        }
        // Url's query encoder keeps opaque cursors and Unicode terms in one
        // parameter. Neither input can choose the production host or route.
        let mut url = reqwest::Url::parse("https://osu.ppy.sh/api/v2/beatmapsets/search")
            .expect("fixed official URL");
        {
            let mut pairs = url.query_pairs_mut();
            pairs
                .append_pair("q", &self.q)
                .append_pair("s", &self.status)
                .append_pair(
                    "sort",
                    if self.sort == "date_desc" {
                        "updated_desc"
                    } else {
                        &self.sort
                    },
                );
            if self.mode != "all" {
                pairs.append_pair("m", &self.mode);
            }
            if let Some(cursor) = &self.cursor {
                pairs.append_pair("cursor_string", cursor);
            }
            if !self.extras.iter().any(|(name, _)| name == "nsfw") {
                pairs.append_pair("nsfw", "false");
            }
            for (name, value) in &self.extras {
                pairs.append_pair(name, value);
            }
            for (name, value) in &self.structured_cursor {
                pairs.append_pair(&format!("cursor[{name}]"), value);
            }
        }
        format!("{}?{}", url.path(), url.query().unwrap_or_default())
    }
}

#[derive(Clone, Serialize)]
struct Difficulty {
    official_id: i64,
    status: String,
    version: String,
    mode: i16,
    stars: f64,
    length_seconds: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    checksum: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_id: Option<i64>,
    archive_available: bool,
    bpm: Option<f64>,
    ar: Option<f64>,
    cs: Option<f64>,
    od: Option<f64>,
    hp: Option<f64>,
    max_combo: Option<i32>,
}
#[derive(Clone, Default, Serialize)]
struct Local {
    stored_count: i64,
    archive_available: bool,
    withdrawn: bool,
}
#[derive(Clone, Serialize)]
struct Item {
    official_set_id: i64,
    title: String,
    artist: String,
    creator: String,
    status: String,
    cover_url: String,
    official_url: String,
    download_disabled: bool,
    archive_download_available: bool,
    archive_download_url: Option<String>,
    difficulties: Vec<Difficulty>,
    local: Local,
    #[serde(skip)]
    native: Value,
    #[serde(skip)]
    catalogue_complete: bool,
}
#[derive(Clone, Serialize)]
struct SearchResult {
    items: Vec<Item>,
    total: i64,
    cursor_string: Option<String>,
    #[serde(skip)]
    native_cursor: Option<Value>,
}

fn native_result(result: &SearchResult) -> Value {
    let beatmapsets = result
        .items
        .iter()
        .filter(|item| !item.local.withdrawn)
        .map(|item| {
            let mut value = item.native.clone();
            let disabled = !item.archive_download_available;
            value["download_disabled"] = json!(disabled);
            value["availability"] = json!({"download_disabled":disabled,"more_information":null});
            value
        })
        .collect::<Vec<_>>();
    json!({"beatmapsets":beatmapsets,"total":result.total,"cursor":result.native_cursor,"cursor_string":result.cursor_string})
}

fn optional_text(value: Option<&Value>, fallback: &str, maximum: usize) -> ApiResult<String> {
    match value {
        None | Some(Value::Null) => Ok(fallback.into()),
        Some(value) => {
            let value = value
                .as_str()
                .filter(|value| value.len() <= maximum)
                .ok_or_else(invalid)?;
            Ok(value
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect::<String>()
                .trim()
                .to_owned())
        }
    }
}
fn optional_integer(value: Option<&Value>) -> ApiResult<i32> {
    match value {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .filter(|value| *value >= 0)
            .ok_or_else(invalid),
    }
}
fn optional_bool(value: Option<&Value>) -> ApiResult<bool> {
    match value {
        None | Some(Value::Null) => Ok(false),
        Some(value) => value.as_bool().ok_or_else(invalid),
    }
}
fn optional_date(value: Option<&Value>) -> ApiResult<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .filter(|value| {
                value.len() <= 64 && chrono::DateTime::parse_from_rfc3339(value).is_ok()
            })
            .map(|value| Some(value.into()))
            .ok_or_else(invalid),
    }
}
fn classification(value: Option<&Value>) -> ApiResult<Value> {
    match value {
        None | Some(Value::Null) => Ok(json!({"id":1,"name":"Unspecified"})),
        Some(value) => {
            let id = optional_integer(value.get("id"))?;
            if !(1..=1000).contains(&id) {
                return Err(invalid());
            }
            let name = text(&value["name"])?;
            if name.len() > 128 {
                return Err(invalid());
            }
            Ok(json!({"id":id,"name":name}))
        }
    }
}
fn status_rank(status: &str) -> i32 {
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
fn native_projection(
    set: &Value,
    set_id: i64,
    status: &str,
    difficulties: &[Difficulty],
) -> ApiResult<Value> {
    let mut maps = HashMap::new();
    for map in set["beatmaps"].as_array().ok_or_else(invalid)? {
        if let Some(id) = map["id"].as_i64() {
            maps.insert(id, map);
        }
    }
    let mut beatmaps = Vec::with_capacity(difficulties.len());
    for map in difficulties {
        let source = maps.get(&map.official_id).ok_or_else(invalid)?;
        let map_status = match source.get("status") {
            None => status,
            Some(value) => value
                .as_str()
                .filter(|value| {
                    matches!(
                        *value,
                        "ranked"
                            | "approved"
                            | "qualified"
                            | "loved"
                            | "pending"
                            | "wip"
                            | "graveyard"
                    )
                })
                .ok_or_else(invalid)?,
        };
        let hit_length = match source.get("hit_length") {
            None => map.length_seconds,
            value => optional_integer(value)?,
        };
        if hit_length > map.length_seconds {
            return Err(invalid());
        }
        let mut native_map = json!({
            "id":map.official_id,"beatmapset_id":set_id,"version":map.version,"checksum":map.checksum.as_deref().unwrap_or_default(),
            "mode_int":map.mode,"mode":(["osu","taiko","fruits","mania"][map.mode as usize]),"difficulty_rating":map.stars,
            "status":map_status,"ranked":status_rank(map_status),"total_length":map.length_seconds,"hit_length":hit_length,
            "bpm":map.bpm.unwrap_or(0.0),"ar":map.ar.unwrap_or(0.0),"cs":map.cs.unwrap_or(0.0),"accuracy":map.od.unwrap_or(0.0),"drain":map.hp.unwrap_or(0.0),
            "max_combo":map.max_combo,"count_circles":optional_integer(source.get("count_circles"))?,
            "count_sliders":optional_integer(source.get("count_sliders"))?,"count_spinners":optional_integer(source.get("count_spinners"))?,
            "playcount":optional_integer(source.get("playcount"))?,"passcount":optional_integer(source.get("passcount"))?,
            "current_user_playcount":0,"last_updated":optional_date(source.get("last_updated"))?,
            "convert":false,"user_id":0,"owners":[],"is_scoreable":true
            ,"lazer_only":optional_bool(source.get("lazer_only"))?
        });
        // Missing optional upstream statistics remain absent rather than being
        // presented as measured zeroes. Native clients apply their DTO defaults.
        for field in [
            "bpm",
            "ar",
            "cs",
            "accuracy",
            "drain",
            "count_circles",
            "count_sliders",
            "count_spinners",
            "playcount",
            "passcount",
            "last_updated",
            "lazer_only",
        ] {
            if source.get(field).is_none_or(Value::is_null) {
                native_map.as_object_mut().unwrap().remove(field);
            }
        }
        beatmaps.push(native_map);
    }
    let mut covers = serde_json::Map::new();
    for name in [
        "cover",
        "cover@2x",
        "card",
        "card@2x",
        "list",
        "list@2x",
        "slimcover",
        "slimcover@2x",
    ] {
        covers.insert(
            name.into(),
            json!(format!(
                "https://assets.ppy.sh/beatmaps/{set_id}/covers/{name}.jpg"
            )),
        );
    }
    let mut result = json!({
        "id":set_id,"title":text(&set["title"])?,"title_unicode":optional_text(set.get("title_unicode"),"",1024)?,
        "artist":text(&set["artist"])?,"artist_unicode":optional_text(set.get("artist_unicode"),"",1024)?,
        "creator":text(&set["creator"])?,"status":status,"ranked":status_rank(status),"user_id":0,
        "source":optional_text(set.get("source"),"",1024)?,"tags":optional_text(set.get("tags"),"",4096)?,
        "bpm":optional_number(set.get("bpm"),100_000.0)?.unwrap_or(0.0),"nsfw":optional_bool(set.get("nsfw"))?,
        "spotlight":optional_bool(set.get("spotlight"))?,"video":optional_bool(set.get("video"))?,"storyboard":optional_bool(set.get("storyboard"))?,
        "play_count":optional_integer(set.get("play_count"))?,"favourite_count":optional_integer(set.get("favourite_count"))?,"has_favourited":false,
        "submitted_date":optional_date(set.get("submitted_date"))?,"last_updated":optional_date(set.get("last_updated"))?,"ranked_date":optional_date(set.get("ranked_date"))?,
        "covers":covers,"preview_url":format!("https://b.ppy.sh/preview/{set_id}.mp3"),
        "genre":classification(set.get("genre"))?,"language":classification(set.get("language"))?,
        "beatmaps":beatmaps,"download_disabled":true,"availability":{"download_disabled":true,"more_information":null}
    });
    for field in [
        "bpm",
        "nsfw",
        "spotlight",
        "video",
        "storyboard",
        "play_count",
        "favourite_count",
        "genre",
        "language",
        "submitted_date",
        "last_updated",
        "ranked_date",
    ] {
        if set.get(field).is_none_or(Value::is_null) {
            result.as_object_mut().unwrap().remove(field);
        }
    }
    Ok(result)
}

fn native_cursor(value: Option<&Value>) -> ApiResult<Option<Value>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(fields)) if fields.len() <= 16 => {
            if fields.is_empty() {
                return Ok(None);
            }
            if !fields.is_empty() && !fields.contains_key("id") {
                return Err(invalid());
            }
            for (name, value) in fields {
                if !cursor_field(name)
                    || !match value {
                        Value::String(value) => {
                            matches!(
                                name.as_str(),
                                "artist.raw"
                                    | "creator.raw"
                                    | "title.raw"
                                    | "approved_date"
                                    | "last_update"
                                    | "queued_at"
                            ) && cursor_query_part(name, value)
                        }
                        Value::Number(value) => {
                            !matches!(name.as_str(), "artist.raw" | "creator.raw" | "title.raw")
                                && cursor_query_part(name, &value.to_string())
                        }
                        _ => false,
                    }
                {
                    return Err(invalid());
                }
            }
            Ok(Some(Value::Object(fields.clone())))
        }
        _ => Err(invalid()),
    }
}
pub(super) struct CachedSearch {
    until: Instant,
    bytes: usize,
    value: Result<SearchResult, CachedFailure>,
}

fn invalid() -> ApiError {
    upstream_error(
        "official_search_invalid",
        "The official search response was invalid.",
    )
}
fn busy() -> ApiError {
    ApiError(
        StatusCode::TOO_MANY_REQUESTS,
        "official_resolution_busy",
        "Official beatmap discovery is busy. Please try shortly.".into(),
    )
}
fn timeout_error() -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "official_search_timeout",
        "Official search could not finish within its deadline. Please try shortly.".into(),
    )
}
fn text(value: &Value) -> ApiResult<String> {
    let value = value
        .as_str()
        .filter(|value| value.len() <= 1024)
        .ok_or_else(invalid)?;
    let value = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>();
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid());
    }
    Ok(value.into())
}
fn id(value: &Value) -> ApiResult<i64> {
    value
        .as_i64()
        .filter(|id| (1..=MAX_OFFICIAL_ID).contains(id))
        .ok_or_else(invalid)
}
fn optional_number(value: Option<&Value>, maximum: f64) -> ApiResult<Option<f64>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .filter(|value| value.is_finite() && (0.0..=maximum).contains(value))
            .map(Some)
            .ok_or_else(invalid),
    }
}
fn transform(value: &Value) -> ApiResult<SearchResult> {
    let sets = value["beatmapsets"]
        .as_array()
        .filter(|sets| sets.len() <= 50)
        .ok_or_else(invalid)?;
    let total = value["total"]
        .as_i64()
        .filter(|total| (0..=i32::MAX as i64).contains(total))
        .ok_or_else(invalid)?;
    let cursor_string = match value.get("cursor_string") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .filter(|value| cursor_valid(value))
                .ok_or_else(invalid)?
                .into(),
        ),
    };
    let native_cursor = native_cursor(value.get("cursor"))?;
    let mut items = Vec::with_capacity(sets.len());
    let mut set_ids = HashSet::new();
    let mut map_ids = HashSet::new();
    for set in sets {
        if set.get("deleted_at").is_some_and(|value| !value.is_null()) {
            continue;
        }
        let set_id = id(&set["id"])?;
        if !set_ids.insert(set_id) {
            return Err(invalid());
        }
        let status = set["status"]
            .as_str()
            .filter(|status| {
                matches!(
                    *status,
                    "ranked" | "approved" | "qualified" | "loved" | "pending" | "wip" | "graveyard"
                )
            })
            .ok_or_else(invalid)?
            .to_owned();
        let maps = set["beatmaps"]
            .as_array()
            .filter(|maps| maps.len() <= 1024)
            .ok_or_else(invalid)?;
        let mut difficulties = Vec::new();
        for map in maps {
            match map.get("convert") {
                Some(Value::Bool(true)) => continue,
                None | Some(Value::Bool(false)) => {}
                _ => return Err(invalid()),
            }
            if map.get("deleted_at").is_some_and(|value| !value.is_null()) {
                continue;
            }
            let official_id = id(&map["id"])?;
            if map["beatmapset_id"].as_i64() != Some(set_id) || !map_ids.insert(official_id) {
                return Err(invalid());
            }
            let mode = map["mode_int"]
                .as_i64()
                .filter(|mode| (0..=3).contains(mode))
                .ok_or_else(invalid)? as i16;
            if map.get("mode").is_some_and(|value| {
                value.as_str() != Some(["osu", "taiko", "fruits", "mania"][mode as usize])
            }) {
                return Err(invalid());
            }
            let stars = map["difficulty_rating"]
                .as_f64()
                .filter(|stars| stars.is_finite() && (0.0..=1000.0).contains(stars))
                .ok_or_else(invalid)?;
            let length_seconds = map["total_length"]
                .as_i64()
                .and_then(|length| i32::try_from(length).ok())
                .filter(|length| *length >= 0)
                .ok_or_else(invalid)?;
            difficulties.push(Difficulty {
                official_id,
                status: match map.get("status") {
                    None => status.clone(),
                    Some(value) => value
                        .as_str()
                        .filter(|status| {
                            matches!(
                                *status,
                                "ranked"
                                    | "approved"
                                    | "qualified"
                                    | "loved"
                                    | "pending"
                                    | "wip"
                                    | "graveyard"
                            )
                        })
                        .ok_or_else(invalid)?
                        .to_owned(),
                },
                version: text(&map["version"])?,
                mode,
                stars,
                length_seconds,
                checksum: match map.get("checksum") {
                    None | Some(Value::Null) => None,
                    Some(value) => Some(
                        value
                            .as_str()
                            .filter(|value| {
                                value.len() == 32 && value.bytes().all(|c| c.is_ascii_hexdigit())
                            })
                            .ok_or_else(invalid)?
                            .to_ascii_lowercase(),
                    ),
                },
                local_id: None,
                archive_available: false,
                bpm: optional_number(map.get("bpm"), 100_000.0)?,
                ar: optional_number(map.get("ar"), 100.0)?,
                cs: optional_number(map.get("cs"), 100.0)?,
                od: optional_number(map.get("accuracy"), 100.0)?,
                hp: optional_number(map.get("drain"), 100.0)?,
                max_combo: match map.get("max_combo") {
                    None | Some(Value::Null) => None,
                    Some(value) => Some(
                        value
                            .as_i64()
                            .and_then(|value| i32::try_from(value).ok())
                            .filter(|value| *value >= 0)
                            .ok_or_else(invalid)?,
                    ),
                },
            });
            if difficulties.len() > 512 {
                return Err(invalid());
            }
        }
        if difficulties.is_empty() {
            continue;
        }
        difficulties.sort_by(|a, b| {
            a.mode
                .cmp(&b.mode)
                .then_with(|| a.stars.total_cmp(&b.stars))
                .then(a.official_id.cmp(&b.official_id))
        });
        let download_disabled = match set.get("availability") {
            None | Some(Value::Null) => true,
            Some(Value::Object(value)) => match value.get("download_disabled") {
                None => true,
                Some(value) => value.as_bool().ok_or_else(invalid)?,
            },
            _ => return Err(invalid()),
        };
        // Public discovery tolerates absent optional fields. Publication requires
        // a complete original-difficulty manifest before using its sanitized projection.
        let mut originals = set.clone();
        originals["beatmaps"] = json!(
            maps.iter()
                .filter(|map| difficulties
                    .iter()
                    .any(|d| map["id"].as_i64() == Some(d.official_id)))
                .collect::<Vec<_>>()
        );
        let catalogue_complete =
            illya_domain::official_set::OfficialSetMetadata::from_upstream(&originals, set_id)
                .is_ok();
        let native = native_projection(set, set_id, &status, &difficulties)?;
        items.push(Item {
            official_set_id: set_id,
            title: text(&set["title"])?,
            artist: text(&set["artist"])?,
            creator: text(&set["creator"])?,
            status,
            cover_url: format!("https://assets.ppy.sh/beatmaps/{set_id}/covers/card.jpg"),
            official_url: format!("https://osu.ppy.sh/beatmapsets/{set_id}"),
            download_disabled,
            archive_download_available: false,
            archive_download_url: None,
            difficulties,
            local: Local::default(),
            native,
            catalogue_complete,
        });
    }
    let result = SearchResult {
        items,
        total,
        cursor_string,
        native_cursor,
    };
    if serde_json::to_vec(&native_result(&result))
        .map_err(ApiError::internal)?
        .len()
        > MAX_NATIVE_RESPONSE_BYTES
    {
        return Err(invalid());
    }
    Ok(result)
}

fn cached(official: &mut OfficialState, key: &SearchKey) -> Option<ApiResult<SearchResult>> {
    official
        .search_cache
        .retain(|_, entry| entry.until > Instant::now());
    official
        .search_cache
        .get(key)
        .map(|entry| match &entry.value {
            Ok(value) => Ok(value.clone()),
            Err(error) => Err(ApiError(error.status, error.code, error.message.clone())),
        })
}
fn remember(official: &mut OfficialState, key: SearchKey, result: &ApiResult<SearchResult>) {
    let (until, bytes, value) = match result {
        Ok(value) => {
            let bytes = serde_json::to_vec(value)
                .map_or(MAX_CACHE_BYTES + 1, |value| value.len())
                .saturating_add(
                    serde_json::to_vec(&native_result(value))
                        .map_or(MAX_CACHE_BYTES + 1, |value| value.len()),
                );
            (Instant::now() + POSITIVE_TTL, bytes, Ok(value.clone()))
        }
        Err(error) => {
            let mut failure = CachedFailure::new(error, Duration::from_secs(5));
            if error.0 == StatusCode::TOO_MANY_REQUESTS
                && let Some(global) = &official.failure
                && global.status == StatusCode::TOO_MANY_REQUESTS
            {
                failure.until = global.until;
            }
            (failure.until, failure.message.len() + 128, Err(failure))
        }
    };
    if bytes > MAX_CACHE_BYTES {
        return;
    }
    official
        .search_cache
        .retain(|_, entry| entry.until > Instant::now());
    official.search_cache.remove(&key);
    while official.search_cache.len() >= MAX_ENTRIES
        || official
            .search_cache
            .values()
            .map(|entry| entry.bytes)
            .sum::<usize>()
            + bytes
            > MAX_CACHE_BYTES
    {
        let Some(oldest) = official
            .search_cache
            .iter()
            .min_by_key(|(_, entry)| entry.until)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        official.search_cache.remove(&oldest);
    }
    official.search_cache.insert(
        key,
        CachedSearch {
            until,
            bytes,
            value,
        },
    );
}
async fn authorize(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    auth::require(state, headers)
        .await?
        .require("beatmaps.import")
}
async fn resolve(
    state: &AppState,
    headers: Option<&HeaderMap>,
    key: &SearchKey,
    user_id: Option<i64>,
) -> ApiResult<SearchResult> {
    if let Some(user_id) = user_id {
        require_active_user(state, user_id).await?;
    }
    let selector = MapSelector::OfficialSearch(key.clone());
    let (flight, slots) = {
        let mut official = state.official.lock().await;
        if let Some(result) = cached(&mut official, key) {
            return result;
        }
        official
            .flights
            .retain(|_, flight| flight.strong_count() > 0);
        let flight = if let Some(flight) = official.flights.get(&selector).and_then(Weak::upgrade) {
            flight
        } else {
            if official.flights.len() >= MAX_RESOLUTION_KEYS {
                return Err(busy());
            }
            let flight = Arc::new(Mutex::new(()));
            official.flights.insert(selector, Arc::downgrade(&flight));
            flight
        };
        (flight, official.slots.clone())
    };
    let _flight = flight.lock().await;
    if let Some(user_id) = user_id {
        require_active_user(state, user_id).await?;
    }
    if let Some(headers) = headers {
        authorize(state, headers).await?;
    }
    {
        let mut official = state.official.lock().await;
        if let Some(result) = cached(&mut official, key) {
            return result;
        }
    }
    let _public_permit = if headers.is_none() {
        let slots = state.official.lock().await.public_discovery_slots.clone();
        Some(slots.try_acquire_owned().map_err(|_| busy())?)
    } else {
        None
    };
    let _permit = slots.try_acquire_owned().map_err(|_| busy())?;
    if headers.is_none() {
        // Public browsing can occupy at most two resolution slots, and consume
        // at most thirty cache misses a minute across all callers. Cached
        // results do not consume this budget; native resolution keeps capacity.
        let mut official = state.official.lock().await;
        official
            .public_discovery_requests
            .retain(|at| at.elapsed() < Duration::from_secs(60));
        if official.public_discovery_requests.len() >= 30 {
            return Err(busy());
        }
        official.public_discovery_requests.push(Instant::now());
    }
    let result = async {
        let response = official_request(state, &key.path(), true).await?;
        let bytes = read_official_bytes(response, 4 * 1024 * 1024).await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if let Some(set_id) = key.set_id {
            if value["id"].as_i64() != Some(set_id) {
                return Err(invalid());
            }
            transform(&json!({"beatmapsets":[value],"total":1,"cursor_string":null}))
        } else {
            transform(&value)
        }
    }
    .await
    .and_then(|result| {
        cursor_complete(&result, key)?;
        Ok(result)
    });
    remember(&mut *state.official.lock().await, key.clone(), &result);
    result
}
async fn merge_local(state: &AppState, result: &mut SearchResult) -> ApiResult<()> {
    let provider_configured = official_archive_download::configured(state).await;
    let ids = result
        .items
        .iter()
        .map(|item| item.official_set_id)
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Ok(());
    }
    // Availability, retained counts and exact difficulty links use one database
    // snapshot. Withdrawal cannot split those projections across two reads.
    let rows = sqlx::query("SELECT candidate.id, (SELECT count(*) FROM beatmaps b WHERE b.official_set_id=candidate.id AND b.source='official') AS stored_count, EXISTS(SELECT 1 FROM official_beatmapset_archives a WHERE a.official_set_id=candidate.id) AS archive_available, NOT illya_beatmapset_active(candidate.id) AS withdrawn, COALESCE((SELECT jsonb_agg(jsonb_build_object('native_id',o.native_id,'status',o.status)) FROM beatmap_status_overrides o WHERE o.native_set_id=candidate.id), '[]'::jsonb) AS status_overrides, COALESCE((SELECT jsonb_agg(jsonb_build_object('official_id',b.official_id,'local_id',b.id,'checksum',b.md5,'archive_available',EXISTS(SELECT 1 FROM official_beatmapset_archive_members m JOIN official_beatmapset_archives a ON a.id=m.archive_id WHERE m.beatmap_id=b.id AND a.official_set_id=candidate.id))) FROM beatmaps b WHERE b.source='official' AND b.official_id IS NOT NULL AND b.official_set_id=candidate.id AND illya_beatmapset_active(b.native_set_id)), '[]'::jsonb) AS stored FROM unnest($1::bigint[]) AS candidate(id)")
        .bind(&ids).fetch_all(&state.db).await?;
    #[derive(Deserialize)]
    struct Stored {
        official_id: i64,
        local_id: i64,
        checksum: String,
        archive_available: bool,
    }
    #[derive(Deserialize)]
    struct StatusOverride {
        native_id: i64,
        status: String,
    }
    let mut local = HashMap::new();
    let mut stored = HashMap::new();
    let mut status_overrides = HashMap::new();
    for row in rows {
        let set_id = row.get::<i64, _>("id");
        local.insert(
            set_id,
            Local {
                stored_count: row.get("stored_count"),
                archive_available: row.get("archive_available"),
                withdrawn: row.get("withdrawn"),
            },
        );
        let members: Vec<Stored> =
            serde_json::from_value(row.get("stored")).map_err(ApiError::internal)?;
        for member in members {
            stored.insert((set_id, member.official_id), member);
        }
        let overrides: Vec<StatusOverride> =
            serde_json::from_value(row.get("status_overrides")).map_err(ApiError::internal)?;
        for value in overrides {
            status_overrides.insert((set_id, value.native_id), value.status);
        }
    }
    for item in &mut result.items {
        item.local = local
            .get(&item.official_set_id)
            .cloned()
            .unwrap_or_default();
        if item.local.withdrawn {
            // Staff can identify a blocked official set without exposing retained
            // internal IDs, paths, metadata or archive availability through cache.
            item.local.stored_count = 0;
            item.local.archive_available = false;
        }
    }
    for item in &mut result.items {
        for map in &mut item.difficulties {
            if let Some(status) = status_overrides.get(&(item.official_set_id, map.official_id)) {
                map.status.clone_from(status);
            }
            let member = stored
                .get(&(item.official_set_id, map.official_id))
                .filter(|_| !item.local.withdrawn)
                .filter(|member| map.checksum.as_ref() == Some(&member.checksum));
            map.local_id = member.map(|member| member.local_id);
            map.archive_available = member.is_some_and(|member| member.archive_available);
        }
        if let Some(maps) = item.native["beatmaps"].as_array_mut() {
            for map in maps {
                if let Some(status) = map["id"]
                    .as_i64()
                    .and_then(|id| status_overrides.get(&(item.official_set_id, id)))
                {
                    let status = if status == "unranked" {
                        "pending"
                    } else {
                        status.as_str()
                    };
                    map["status"] = json!(status);
                    map["ranked"] = json!(status_rank(status));
                }
            }
        }
        // A mixed set retains its upstream set label. A common difficulty
        // status can truthfully describe the complete set after moderation.
        if item
            .difficulties
            .iter()
            .any(|map| status_overrides.contains_key(&(item.official_set_id, map.official_id)))
            && let Some(first) = item.difficulties.first()
            && item
                .difficulties
                .iter()
                .all(|map| map.status == first.status)
        {
            item.status.clone_from(&first.status);
            let status = if first.status == "unranked" {
                "pending"
            } else {
                first.status.as_str()
            };
            item.native["status"] = json!(status);
            item.native["ranked"] = json!(status_rank(status));
        }
    }
    for item in &mut result.items {
        item.archive_download_available = !item.local.withdrawn
            && (item.local.archive_available || (provider_configured && !item.download_disabled));
        item.archive_download_url = if !item.archive_download_available {
            None
        } else if let Some(id) = item
            .difficulties
            .iter()
            .find(|map| map.archive_available)
            .and_then(|map| map.local_id)
        {
            Some(format!("/api/v1/beatmaps/{id}/archive"))
        } else {
            Some(format!(
                "/api/v1/official/beatmapsets/{}/archive",
                item.official_set_id
            ))
        };
    }
    Ok(())
}
pub async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> ApiResult {
    authorize(&state, &headers).await?;
    let key = parse(raw)?;
    let result = tokio::time::timeout(OPERATION_DEADLINE, async {
        let mut result = resolve(&state, Some(&headers), &key, None).await?;
        authorize(&state, &headers).await?;
        merge_local(&state, &mut result).await?;
        authorize(&state, &headers).await?;
        Ok::<_, ApiError>(result)
    })
    .await;
    authorize(&state, &headers).await?;
    let result = match result {
        Ok(result) => result?,
        Err(_) => {
            let error = timeout_error();
            if let Ok(mut official) = state.official.try_lock()
                && cached(&mut official, &key).is_none_or(|result| result.is_err())
            {
                remember(
                    &mut official,
                    key,
                    &Err(ApiError(error.0, error.1, error.2.clone())),
                );
            }
            return Err(error);
        }
    };
    Ok(Json(
        serde_json::to_value(result).map_err(ApiError::internal)?,
    ))
}

/// Complete, sanitized, unmerged public metadata for an atomic rank decision.
/// Uses the same bounded upstream flight, cache and rate budget as discovery.
pub(crate) async fn catalogue_snapshot(state: &AppState, set_id: i64) -> ApiResult<Value> {
    if !(1..=MAX_OFFICIAL_ID).contains(&set_id) {
        return Err(ApiError::bad("Supply an official beatmap set ID."));
    }
    let mut key = parse(None)?;
    key.set_id = Some(set_id);
    let result = tokio::time::timeout(OPERATION_DEADLINE, resolve(state, None, &key, None))
        .await
        .map_err(|_| timeout_error())??;
    let item = result
        .items
        .iter()
        .find(|item| item.official_set_id == set_id)
        .filter(|item| item.catalogue_complete)
        .ok_or_else(invalid)?;
    let mut metadata = item.native.clone();
    metadata["download_disabled"] = json!(item.download_disabled);
    metadata["availability"] =
        json!({"download_disabled":item.download_disabled,"more_information":null});
    illya_domain::official_set::OfficialSetMetadata::from_upstream(&metadata, set_id)
        .map_err(|_| invalid())?;
    Ok(metadata)
}

async fn public_result(state: &AppState, key: SearchKey) -> ApiResult<SearchResult> {
    let deadline = tokio::time::Instant::now() + OPERATION_DEADLINE;
    if let Some(set_id) = key.set_id {
        tokio::time::timeout_at(
            deadline,
            availability::require_active_set(&state.db, set_id),
        )
        .await
        .map_err(|_| timeout_error())??;
    }
    let mut result = match tokio::time::timeout_at(deadline, resolve(state, None, &key, None)).await
    {
        Ok(result) => result?,
        Err(_) => {
            let error = timeout_error();
            if let Ok(mut official) = state.official.try_lock()
                && cached(&mut official, &key).is_none_or(|result| result.is_err())
            {
                remember(
                    &mut official,
                    key,
                    &Err(ApiError(error.0, error.1, error.2.clone())),
                );
            }
            return Err(error);
        }
    };
    // Local SQL has the same overall deadline, but its failures must never
    // replace a healthy shared upstream cache entry.
    tokio::time::timeout_at(deadline, async {
        merge_local(state, &mut result).await?;
        result.items.retain(|item| !item.local.withdrawn);
        if let Some(set_id) = key.set_id {
            availability::require_active_set(&state.db, set_id).await?;
        }
        Ok(result)
    })
    .await
    .map_err(|_| timeout_error())?
}

pub async fn browse(State(state): State<AppState>, RawQuery(raw): RawQuery) -> ApiResult {
    let result = public_result(&state, parse(raw)?).await?;
    Ok(Json(
        serde_json::to_value(result).map_err(ApiError::internal)?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeInput {
    user_id: i64,
    query: String,
}

pub async fn native_search(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
) -> ApiResult {
    require_service_key(&state, &headers)?;
    let bytes = to_bytes(request.into_body(), 16 * 1024)
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "The search request exceeds its size limit.".into(),
            )
        })?;
    let input: NativeInput = serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::bad("Supply a user ID and bounded native search query."))?;
    if input.user_id <= 0 {
        return Err(ApiError::bad("Supply a positive user ID."));
    }
    require_active_user(&state, input.user_id).await?;
    let key = native_parse(&input.query)?;
    let operation = tokio::time::timeout(OPERATION_DEADLINE, async {
        let result = resolve(&state, None, &key, Some(input.user_id)).await;
        // Restriction wins over cached failures and network/flight errors.
        require_active_user(&state, input.user_id).await?;
        let mut result = result?;
        merge_local(&state, &mut result).await?;
        require_active_user(&state, input.user_id).await?;
        Ok::<_, ApiError>(native_result(&result))
    })
    .await;
    require_service_key(&state, &headers)?;
    require_active_user(&state, input.user_id).await?;
    let result = match operation {
        Ok(result) => result?,
        Err(_) => {
            let error = timeout_error();
            if let Ok(mut official) = state.official.try_lock()
                && cached(&mut official, &key).is_none_or(|result| result.is_err())
            {
                remember(
                    &mut official,
                    key,
                    &Err(ApiError(error.0, error.1, error.2.clone())),
                );
            }
            return Err(error);
        }
    };
    Ok(Json(result))
}

pub async fn detail(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult {
    if !(1..=MAX_OFFICIAL_ID).contains(&id) {
        return Err(ApiError::bad("Supply one valid official set ID."));
    }
    let mut key = parse(None)?;
    key.set_id = Some(id);
    let result = public_result(&state, key).await?;
    let item = result
        .items
        .into_iter()
        .next()
        .ok_or_else(ApiError::missing)?;
    Ok(Json(
        serde_json::to_value(item).map_err(ApiError::internal)?,
    ))
}

/// Moderation needs verified upstream membership even while a set is withdrawn.
/// Only the bounded upstream DTO crosses this boundary; retained content and
/// availability are projected separately by the authenticated staff handler.
pub(crate) async fn staff_detail(
    state: &AppState,
    headers: &HeaderMap,
    id: i64,
) -> ApiResult<Value> {
    auth::require(state, headers)
        .await?
        .require("beatmaps.moderate")?;
    if !(1..=MAX_OFFICIAL_ID).contains(&id) {
        return Err(ApiError::bad("Supply one valid official set ID."));
    }
    let mut key = parse(None)?;
    key.set_id = Some(id);
    // Keep the shared flight, response-size, rate-spacing and public discovery
    // capacity bounds. The staff exception applies only to the active-set gate.
    let result = tokio::time::timeout(OPERATION_DEADLINE, resolve(state, None, &key, None)).await;
    // Revocation or actual session expiry wins over upstream/cache failures.
    auth::require(state, headers)
        .await?
        .require("beatmaps.moderate")?;
    let item = result
        .map_err(|_| timeout_error())??
        .items
        .into_iter()
        .next()
        .ok_or_else(ApiError::missing)?;
    serde_json::to_value(item).map_err(ApiError::internal)
}

#[cfg(test)]
#[path = "official_search_tests.rs"]
mod tests;
