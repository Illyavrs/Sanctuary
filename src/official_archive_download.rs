//! Verified, transient official archives. This module never publishes a map,
//! invents an uploader, writes a catalogue row, or forwards a player's headers.
use super::*;
use axum::{
    body::{Body, Bytes, to_bytes},
    extract::Request,
};

const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 2;
const MAX_FLIGHTS: usize = 32;
const DEADLINE: Duration = Duration::from_secs(40);
const TTL: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct Config {
    template: String,
    hosts: HashSet<String>,
    client: reqwest::Client,
    #[cfg(test)]
    loopback_fixture: bool,
}
struct CachedArchive {
    bytes: Bytes,
    until: Instant,
}
pub(super) struct DownloadState {
    config: Option<Config>,
    cache: HashMap<i64, CachedArchive>,
    failures: HashMap<i64, CachedFailure>,
    flights: HashMap<i64, Weak<Mutex<()>>>,
    slots: Arc<tokio::sync::Semaphore>,
    requests: Vec<Instant>,
    backoff: Option<Instant>,
}
impl Default for DownloadState {
    fn default() -> Self {
        Self {
            config: None,
            cache: HashMap::new(),
            failures: HashMap::new(),
            flights: HashMap::new(),
            slots: Arc::new(tokio::sync::Semaphore::new(2)),
            requests: Vec::new(),
            backoff: None,
        }
    }
}
fn unavailable() -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "official_archive_unavailable",
        "A complete official archive is not available from this server.".into(),
    )
}
fn busy() -> ApiError {
    ApiError(
        StatusCode::TOO_MANY_REQUESTS,
        "official_archive_rate_limited",
        "Official archive downloads are busy. Please try later.".into(),
    )
}
fn failed() -> ApiError {
    ApiError(
        StatusCode::BAD_GATEWAY,
        "official_archive_provider_failed",
        "The archive provider could not complete this download.".into(),
    )
}
fn invalid() -> ApiError {
    ApiError(
        StatusCode::BAD_GATEWAY,
        "official_archive_invalid",
        "The provider archive did not match a complete playable official set.".into(),
    )
}
fn timeout_error() -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "official_archive_timeout",
        "The official archive could not be verified within its deadline. Please try later.".into(),
    )
}

fn public_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.ends_with('.')
        && host.parse::<std::net::IpAddr>().is_err()
        && host.contains('.')
        && !host.to_ascii_lowercase().ends_with(".localhost")
        && !host.to_ascii_lowercase().ends_with(".local")
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}
impl Config {
    fn new(template: String, redirect_hosts: &str) -> Result<Self, String> {
        let error = || "Invalid OSU_ARCHIVE_URL_TEMPLATE or OSU_ARCHIVE_REDIRECT_HOSTS.".to_owned();
        if template.len() > 2048 || template.matches("{set_id}").count() != 1 {
            return Err(error());
        }
        let url = reqwest::Url::parse(&template.replace("{set_id}", "1")).map_err(|_| error())?;
        let host = url.host_str().ok_or_else(error)?;
        if url.scheme() != "https"
            || !public_host(host)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.port_or_known_default() != Some(443)
            || !template
                .split('?')
                .next()
                .unwrap_or_default()
                .contains("{set_id}")
            || template
                .split("://")
                .nth(1)
                .unwrap_or_default()
                .split('/')
                .next()
                .unwrap_or_default()
                .contains("{set_id}")
        {
            return Err(error());
        }
        let mut hosts = HashSet::from([host.to_ascii_lowercase()]);
        for host in redirect_hosts
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !public_host(host) || hosts.len() >= 8 {
                return Err(error());
            }
            hosts.insert(host.to_ascii_lowercase());
        }
        Ok(Self {
            template,
            hosts,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(25))
                .build()
                .map_err(|_| error())?,
            #[cfg(test)]
            loopback_fixture: false,
        })
    }
    fn allowed(&self, url: &reqwest::Url) -> bool {
        #[cfg(test)]
        if self.loopback_fixture {
            return url.scheme() == "http"
                && url.host_str() == Some("127.0.0.1")
                && url.port()
                    == reqwest::Url::parse(&self.template.replace("{set_id}", "1"))
                        .ok()
                        .and_then(|v| v.port())
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none();
        }
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url
                .host_str()
                .is_some_and(|host| self.hosts.contains(&host.to_ascii_lowercase()))
    }
}
impl OfficialState {
    pub(crate) fn configure_archive_provider(&mut self) -> Result<(), String> {
        let template = std::env::var("OSU_ARCHIVE_URL_TEMPLATE").unwrap_or_default();
        if template.trim().is_empty() {
            return Ok(());
        }
        self.archive_download.config = Some(Config::new(
            template,
            &std::env::var("OSU_ARCHIVE_REDIRECT_HOSTS").unwrap_or_default(),
        )?);
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn fixture_archive_provider(&mut self, template: String) {
        self.archive_download.config = Some(Config {
            template,
            hosts: HashSet::new(),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            loopback_fixture: true,
        });
    }
    #[cfg(test)]
    pub(crate) fn expire_archive_download_failure(&mut self, set_id: i64) {
        if let Some(failure) = self.archive_download.failures.get_mut(&set_id) {
            failure.until = Instant::now();
        }
    }
}
pub(super) async fn configured(state: &AppState) -> bool {
    state
        .official
        .lock()
        .await
        .archive_download
        .config
        .is_some()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    official_set_id: i64,
    #[serde(default, deserialize_with = "supplied_user_id")]
    user_id: Option<i64>,
}
fn supplied_user_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<i64>, D::Error> {
    i64::deserialize(deserializer).map(Some)
}
fn validate_id(set_id: i64) -> ApiResult<()> {
    if set_id <= 0 {
        return Err(ApiError::bad("Supply a positive official set ID."));
    }
    if set_id > MAX_OFFICIAL_ID {
        return Err(official_namespace_conflict());
    }
    Ok(())
}
fn has_identity(headers: &HeaderMap) -> bool {
    headers.contains_key(header::AUTHORIZATION)
        || headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .any(|part| part.trim().starts_with("illya_session="))
            })
}
pub async fn public(
    State(state): State<AppState>,
    Path(set_id): Path<i64>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    validate_id(set_id)?;
    let actor = if has_identity(&headers) {
        Some(auth::require(&state, &headers).await?)
    } else {
        None
    };
    let result = match tokio::time::timeout(
        DEADLINE,
        acquire(&state, set_id, actor.as_ref().map(|u| u.id)),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(timeout_error()),
    };
    let result = final_result(&state, set_id, actor.as_ref().map(|u| u.id), result).await;
    if let Some(actor) = actor {
        let fresh = auth::require(&state, &headers).await?;
        if fresh.id != actor.id || fresh.session != actor.session {
            return Err(ApiError::unauthorized());
        }
    }
    availability::require_active_set(&state.db, set_id).await?;
    response(set_id, result?)
}
pub async fn internal(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
) -> ApiResult<Response> {
    require_service_key(&state, &headers)?;
    let bytes = to_bytes(request.into_body(), 4 * 1024).await.map_err(|_| {
        ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "The archive request is too large.".into(),
        )
    })?;
    let input: Input = serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::bad("Supply one official set ID and an optional user ID."))?;
    validate_id(input.official_set_id)?;
    if input.user_id.is_some_and(|id| id <= 0) {
        return Err(ApiError::bad("Supply a positive user ID."));
    }
    if let Some(id) = input.user_id {
        require_active_user(&state, id).await?;
    }
    let result = match tokio::time::timeout(
        DEADLINE,
        acquire(&state, input.official_set_id, input.user_id),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(timeout_error()),
    };
    let result = final_result(&state, input.official_set_id, input.user_id, result).await;
    require_service_key(&state, &headers)?;
    if let Some(id) = input.user_id {
        require_active_user(&state, id).await?;
    }
    availability::require_active_set(&state.db, input.official_set_id).await?;
    response(input.official_set_id, result?)
}
fn response(set_id: i64, bytes: Bytes) -> ApiResult<Response> {
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let filename = archive_filename(set_id, &bytes)?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-osu-archive".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                illya_domain::download_filename::attachment(&filename),
            ),
            (header::CACHE_CONTROL, "private, no-store".to_owned()),
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff".to_owned(),
            ),
            (
                header::HeaderName::from_static("x-illya-archive-sha256"),
                hash,
            ),
        ],
        Body::from(bytes),
    )
        .into_response())
}
fn archive_filename(set_id: i64, bytes: &[u8]) -> ApiResult<String> {
    use std::io::Read;
    // These bytes have already passed official identity/member verification.
    // Read one bounded chart solely for its display name; preserve every ZIP
    // byte and never ask the provider or metadata API again for a filename.
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| invalid())?;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|_| invalid())?;
        if !entry.name().to_ascii_lowercase().ends_with(".osu") {
            continue;
        }
        if entry.size() > 8 * 1024 * 1024 {
            return Err(invalid());
        }
        let mut text = String::new();
        entry
            .take(8 * 1024 * 1024 + 1)
            .read_to_string(&mut text)
            .map_err(|_| invalid())?;
        if text.len() > 8 * 1024 * 1024 {
            return Err(invalid());
        }
        let mut metadata = false;
        let mut artist = None;
        let mut title = None;
        for line in text.trim_start_matches('\u{feff}').lines().map(str::trim) {
            if line.starts_with('[') && line.ends_with(']') {
                metadata = line == "[Metadata]";
            } else if metadata && let Some((key, value)) = line.split_once(':') {
                match key.trim() {
                    "Artist" => artist = Some(value.trim()),
                    "Title" => title = Some(value.trim()),
                    _ => {}
                }
            }
        }
        return illya_domain::download_filename::archive_filename(
            set_id,
            artist
                .filter(|value| !value.is_empty())
                .ok_or_else(invalid)?,
            title
                .filter(|value| !value.is_empty())
                .ok_or_else(invalid)?,
        )
        .ok_or_else(invalid);
    }
    Err(invalid())
}
async fn authority(state: &AppState, set_id: i64, user_id: Option<i64>) -> ApiResult<()> {
    if let Some(id) = user_id {
        require_active_user(state, id).await?;
    }
    availability::require_active_set(&state.db, set_id).await
}
async fn final_result(
    state: &AppState,
    set_id: i64,
    user_id: Option<i64>,
    result: ApiResult<Bytes>,
) -> ApiResult<Bytes> {
    authority(state, set_id, user_id).await?;
    // An authenticated operator import may commit while metadata/provider work
    // is waiting. Newly retained bytes take precedence over both transient
    // success and failure; discovery never overwrites that publication.
    let retained = retained(state, set_id).await?;
    authority(state, set_id, user_id).await?;
    match retained {
        Some(bytes) => Ok(bytes),
        None => result,
    }
}
async fn stored_versions(
    state: &AppState,
    metadata: &illya_domain::official_set::OfficialSetMetadata,
) -> ApiResult<()> {
    let rows = sqlx::query(
        "SELECT official_id,md5,mode FROM beatmaps WHERE source='official' AND official_set_id=$1",
    )
    .bind(metadata.id)
    .fetch_all(&state.db)
    .await?;
    for row in rows {
        if !metadata.beatmaps.iter().any(|m| {
            Some(m.id) == row.get::<Option<i64>, _>("official_id")
                && m.checksum == row.get::<String, _>("md5")
                && m.mode_int == row.get::<i16, _>("mode")
        }) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "official_set_version_conflict",
                "Stored difficulties belong to a different version of this official set.".into(),
            ));
        }
    }
    Ok(())
}
async fn retained(state: &AppState, set_id: i64) -> ApiResult<Option<Bytes>> {
    let row = sqlx::query("SELECT archive_path,byte_length,sha256 FROM official_beatmapset_archives WHERE official_set_id=$1")
        .bind(set_id).fetch_optional(&state.db).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let path: String = row.get("archive_path");
    let length: i64 = row.get("byte_length");
    if length <= 0
        || length > MAX_BYTES as i64
        || std::path::Path::new(&path).is_absolute()
        || path.contains("..")
        || path.contains(':')
        || path.contains('\\')
    {
        return Err(invalid());
    }
    let path = state.maps_root.join(path);
    let info = tokio::fs::symlink_metadata(&path)
        .await
        .map_err(|_| unavailable())?;
    if !info.is_file() || info.len() != length as u64 {
        return Err(invalid());
    }
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| unavailable())?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| unavailable())?;
    if bytes.len() != length as usize
        || format!("{:x}", Sha256::digest(&bytes)) != row.get::<String, _>("sha256")
    {
        return Err(invalid());
    }
    Ok(Some(Bytes::from(bytes)))
}
async fn acquire(state: &AppState, set_id: i64, user_id: Option<i64>) -> ApiResult<Bytes> {
    authority(state, set_id, user_id).await?;
    // Retained operator imports keep their existing offline-readable contract.
    if let Some(bytes) = retained(state, set_id).await? {
        authority(state, set_id, user_id).await?;
        return Ok(bytes);
    }
    let (config, flight, slots) = {
        let mut official = state.official.lock().await;
        let download = &mut official.archive_download;
        let config = download.config.clone().ok_or_else(unavailable)?;
        download.flights.retain(|_, v| v.strong_count() > 0);
        let flight = if let Some(value) = download.flights.get(&set_id).and_then(Weak::upgrade) {
            value
        } else {
            if download.flights.len() >= MAX_FLIGHTS {
                return Err(busy());
            }
            let value = Arc::new(Mutex::new(()));
            download.flights.insert(set_id, Arc::downgrade(&value));
            value
        };
        (config, flight, download.slots.clone())
    };
    let _flight = flight.lock().await;
    let permit = slots.try_acquire_owned().map_err(|_| busy())?;
    authority(state, set_id, user_id).await?;
    let metadata = official_set_metadata::download_metadata(state, set_id).await?;
    if metadata.download_disabled {
        return Err(unavailable());
    }
    stored_versions(state, &metadata).await?;
    let cached = {
        let mut official = state.official.lock().await;
        let download = &mut official.archive_download;
        download.cache.retain(|_, v| v.until > Instant::now());
        download.failures.retain(|_, v| v.until > Instant::now());
        if let Some(error) = download
            .failures
            .get(&set_id)
            .and_then(CachedFailure::error)
        {
            return Err(error);
        }
        download.cache.get(&set_id).map(|v| v.bytes.clone())
    };
    let bytes = match cached {
        Some(bytes) => bytes,
        None => {
            let now = Instant::now();
            {
                let mut official = state.official.lock().await;
                let download = &mut official.archive_download;
                download
                    .requests
                    .retain(|v| now.duration_since(*v) < Duration::from_secs(60));
                if download.backoff.is_some_and(|v| v > now) || download.requests.len() >= 12 {
                    return Err(busy());
                }
                download.requests.push(now);
            }
            match fetch(&config, set_id).await {
                Ok(bytes) => bytes,
                Err((error, retry)) => {
                    let mut official = state.official.lock().await;
                    let download = &mut official.archive_download;
                    if let Some(retry) = retry {
                        download.backoff = Some(Instant::now() + retry);
                    }
                    if download.failures.len() >= MAX_FLIGHTS {
                        download.failures.clear();
                    }
                    download.failures.insert(
                        set_id,
                        CachedFailure::new(&error, retry.unwrap_or(Duration::from_secs(5))),
                    );
                    return Err(error);
                }
            }
        }
    };
    // Re-fetch policy and original identities after both a download and a cache
    // hit. A disabled, changed, withdrawn or restricted set is never served.
    let fresh = official_set_metadata::download_metadata(state, set_id).await?;
    if fresh.download_disabled {
        return Err(unavailable());
    }
    let value = serde_json::to_value(&fresh).map_err(ApiError::internal)?;
    let parsing = bytes.clone();
    let verified = tokio::task::spawn_blocking(move || {
        // Blocking decompression cannot be aborted by the HTTP deadline. Keep
        // its concurrency permit until it actually finishes, including timeout.
        let _permit = permit;
        official_archive::verify_download(&parsing, set_id, &value)
    })
    .await
    .map_err(ApiError::internal)?;
    if verified.is_err() {
        let error = invalid();
        let mut official = state.official.lock().await;
        let download = &mut official.archive_download;
        download.cache.remove(&set_id);
        if download.failures.len() >= MAX_FLIGHTS {
            download.failures.clear();
        }
        download
            .failures
            .insert(set_id, CachedFailure::new(&error, Duration::from_secs(5)));
        return Err(error);
    }
    stored_versions(state, &fresh).await?;
    authority(state, set_id, user_id).await?;
    {
        let mut official = state.official.lock().await;
        let download = &mut official.archive_download;
        while download.cache.len() >= MAX_CACHE_ENTRIES
            || download
                .cache
                .values()
                .map(|v| v.bytes.len())
                .sum::<usize>()
                + bytes.len()
                > MAX_CACHE_BYTES
        {
            let Some(oldest) = download
                .cache
                .iter()
                .min_by_key(|(_, v)| v.until)
                .map(|(id, _)| *id)
            else {
                break;
            };
            download.cache.remove(&oldest);
        }
        download.cache.insert(
            set_id,
            CachedArchive {
                bytes: bytes.clone(),
                until: Instant::now() + TTL,
            },
        );
    }
    Ok(bytes)
}
async fn fetch(config: &Config, set_id: i64) -> Result<Bytes, (ApiError, Option<Duration>)> {
    let mut url = reqwest::Url::parse(&config.template.replace("{set_id}", &set_id.to_string()))
        .map_err(|_| (failed(), None))?;
    for redirects in 0..=3 {
        if !config.allowed(&url) {
            return Err((failed(), None));
        }
        let response = config
            .client
            .get(url.clone())
            .header(header::ACCEPT_ENCODING, "identity")
            .header(
                header::ACCEPT,
                "application/x-osu-archive, application/zip, application/octet-stream",
            )
            .send()
            .await
            .map_err(|_| (failed(), None))?;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            let seconds = response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(30)
                .clamp(1, 300);
            return Err((busy(), Some(Duration::from_secs(seconds))));
        }
        if response.status().is_redirection() {
            if redirects == 3 {
                return Err((failed(), None));
            }
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .filter(|v| v.len() <= 2048)
                .ok_or_else(|| (failed(), None))?;
            url = url.join(location).map_err(|_| (failed(), None))?;
            continue;
        }
        if response.status() != StatusCode::OK
            || response
                .content_length()
                .is_some_and(|n| n > MAX_BYTES as u64)
        {
            return Err((failed(), None));
        }
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| (failed(), None))? {
            if chunk.len() > MAX_BYTES - bytes.len() {
                return Err((invalid(), None));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            return Err((invalid(), None));
        }
        return Ok(Bytes::from(bytes));
    }
    Err((failed(), None))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archive_provider_configuration_accepts_only_fixed_https_numeric_paths() {
        for template in [
            "",
            "https://provider.example/download",
            "http://provider.example/{set_id}",
            "https://127.0.0.1/{set_id}",
            "https://localhost/{set_id}",
            "https://app.local/{set_id}",
            "https://user:secret@provider.example/{set_id}",
            "https://provider.example:8443/{set_id}",
            "https://{set_id}.provider.example/download",
            "https://provider.example/download?id={set_id}",
            "https://provider.example/{set_id}#fragment",
            "https://provider.example/{set_id}/{set_id}",
        ] {
            assert!(Config::new(template.into(), "").is_err(), "{template}");
        }
        let configured = Config::new(
            "https://provider.example/d/{set_id}?video=1".into(),
            "cdn.example",
        )
        .unwrap();
        assert!(configured.allowed(&reqwest::Url::parse("https://provider.example/d/42").unwrap()));
        assert!(configured.allowed(&reqwest::Url::parse("https://cdn.example/d/42").unwrap()));
        for url in [
            "http://provider.example/d/42",
            "https://outsider.example/d/42",
            "https://cdn.example:8443/d/42",
            "https://user:secret@cdn.example/d/42",
            "https://127.0.0.1/d/42",
            "https://cdn.example/d/42#x",
        ] {
            assert!(
                !configured.allowed(&reqwest::Url::parse(url).unwrap()),
                "{url}"
            );
        }
        for hosts in [
            "127.0.0.1",
            "localhost",
            "user@cdn.example",
            "https://cdn.example",
            "cdn.example:443",
            "a.example,b.example,c.example,d.example,e.example,f.example,g.example,h.example",
        ] {
            assert!(
                Config::new("https://provider.example/{set_id}".into(), hosts).is_err(),
                "{hosts}"
            );
        }
    }
    #[test]
    fn official_archive_identity_namespace_is_not_player_configurable() {
        for id in [-1, 0, MAX_OFFICIAL_ID + 1, i64::MAX] {
            assert!(validate_id(id).is_err());
        }
        for id in [1, 42, MAX_OFFICIAL_ID] {
            validate_id(id).unwrap();
        }
        for body in [
            r#"{"official_set_id":42,"url":"https://outsider.example/"}"#,
            r#"{"official_set_id":42,"official_set_id":43}"#,
            r#"{"official_set_id":"42"}"#,
        ] {
            assert!(serde_json::from_str::<Input>(body).is_err());
        }
        let public: Input = serde_json::from_str(r#"{"official_set_id":42}"#).unwrap();
        assert_eq!(public.user_id, None);
    }
}
