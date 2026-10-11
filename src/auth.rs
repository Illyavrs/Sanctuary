use crate::{ApiError, ApiResult, AppState, scores};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    Json,
    extract::{Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Duration, Utc};
use illya_domain::{
    Preferences, ScoreVariant, has_permission, validate_color, validate_password, validate_username,
};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
struct Claims {
    sub: i64,
    sid: Uuid,
    exp: usize,
}
pub struct User {
    pub id: i64,
    pub session: Uuid,
    pub permissions: Vec<String>,
}
impl User {
    pub fn require(&self, permission: &str) -> ApiResult<()> {
        if has_permission(&self.permissions, permission) {
            Ok(())
        } else {
            Err(ApiError::forbidden())
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum MutationScope {
    Session,
    Permissions,
    RoleChanges,
}

/// Authorization held by the transaction that acquired it. The original session
/// deadline remains authoritative even when this operation deliberately revokes
/// its own session or removes its own permissions.
pub(crate) struct MutationActor {
    user: User,
    expires_at: DateTime<Utc>,
}

impl std::ops::Deref for MutationActor {
    type Target = User;

    fn deref(&self) -> &Self::Target {
        &self.user
    }
}

/// Acquire before resource locks: permission tables, sorted users, then session.
/// Verification issuance is the one recipient-limiter-first caller; it requires
/// no permission-table fence. Never upgrade a Permissions scope to RoleChanges.
pub(crate) async fn mutation_actor(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    snapshot: &User,
    extra_user_ids: &[i64],
    scope: MutationScope,
) -> ApiResult<MutationActor> {
    match scope {
        MutationScope::Session => {}
        MutationScope::Permissions => {
            sqlx::query("LOCK TABLE roles,user_roles IN SHARE MODE")
                .execute(&mut **tx)
                .await?;
        }
        MutationScope::RoleChanges => {
            sqlx::query("LOCK TABLE roles,user_roles IN SHARE ROW EXCLUSIVE MODE")
                .execute(&mut **tx)
                .await?;
        }
    }
    let mut users = Vec::with_capacity(extra_user_ids.len() + 1);
    users.push(snapshot.id);
    users.extend_from_slice(extra_user_ids);
    users.sort_unstable();
    users.dedup();
    // Fence every user update while allowing foreign-key KEY SHARE checks on
    // notification recipients. UPDATE locks would deadlock concurrent fan-out.
    sqlx::query("SELECT id FROM users WHERE id=ANY($1) ORDER BY id FOR NO KEY UPDATE")
        .bind(&users)
        .fetch_all(&mut **tx)
        .await?;
    // A WHERE deadline predicate may be evaluated before a queued, unchanged
    // session row lock returns. Fetch unconditionally and check the clock after.
    let expires_at = sqlx::query_scalar::<_, DateTime<Utc>>(
        "SELECT expires_at FROM sessions WHERE id=$1 AND user_id=$2 FOR UPDATE",
    )
    .bind(snapshot.session)
    .bind(snapshot.id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(ApiError::unauthorized)?;
    let valid = sqlx::query_scalar::<_, bool>(
        "SELECT NOT is_restricted AND $1::timestamptz>clock_timestamp() FROM users WHERE id=$2",
    )
    .bind(expires_at)
    .bind(snapshot.id)
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(false);
    if !valid {
        return Err(ApiError::unauthorized());
    }
    let permissions = match scope {
        MutationScope::Session => Vec::new(),
        MutationScope::Permissions | MutationScope::RoleChanges => {
            sqlx::query_scalar::<_, String>(
                "SELECT DISTINCT unnest(r.permissions) FROM roles r JOIN user_roles ur ON ur.role_id=r.id WHERE ur.user_id=$1",
            )
            .bind(snapshot.id)
            .fetch_all(&mut **tx)
            .await?
        }
    };
    Ok(MutationActor {
        user: User {
            id: snapshot.id,
            session: snapshot.session,
            permissions,
        },
        expires_at,
    })
}

/// Recheck after resource waits and immediately before publication/commit.
/// Held fences block external revocation; only natural expiry can advance.
pub(crate) async fn ensure_mutation_actor(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: &MutationActor,
) -> ApiResult<()> {
    let valid: bool = sqlx::query_scalar("SELECT $1::timestamptz>clock_timestamp()")
        .bind(actor.expires_at)
        .fetch_one(&mut **tx)
        .await?;
    if valid {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
    }
}

pub async fn require(state: &AppState, headers: &HeaderMap) -> ApiResult<User> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| {
            headers
                .get(header::COOKIE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| {
                    v.split(';')
                        .find_map(|part| part.trim().strip_prefix("illya_session="))
                })
        })
        .ok_or_else(ApiError::unauthorized)?;
    let claims = decode::<Claims>(
        token,
        &DecodingKey::from_secret(state.secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|_| ApiError::unauthorized())?
    .claims;
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.id=$1 AND s.user_id=$2 AND s.expires_at>now() AND NOT u.is_restricted)")
        .bind(claims.sid).bind(claims.sub).fetch_one(&state.db).await?;
    if !valid {
        return Err(ApiError::unauthorized());
    }
    let permissions=sqlx::query_scalar::<_,String>("SELECT DISTINCT unnest(r.permissions) FROM roles r JOIN user_roles ur ON ur.role_id=r.id WHERE ur.user_id=$1")
        .bind(claims.sub).fetch_all(&state.db).await?;
    Ok(User {
        id: claims.sub,
        session: claims.sid,
        permissions,
    })
}
pub async fn optional(state: &AppState, headers: &HeaderMap) -> ApiResult<Option<User>> {
    if !headers.contains_key(header::AUTHORIZATION)
        && !headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("illya_session="))
    {
        return Ok(None);
    }
    match require(state, headers).await {
        Ok(user) => Ok(Some(user)),
        Err(e) if e.0 == StatusCode::UNAUTHORIZED => Ok(None),
        Err(e) => Err(e),
    }
}
pub async fn profile(state: &AppState, id: i64, private: bool) -> ApiResult<Value> {
    profile_in_variant(state, id, private, 0, ScoreVariant::Vanilla).await
}
pub(crate) async fn profile_in_variant(
    state: &AppState,
    id: i64,
    private: bool,
    mode: i16,
    variant: ScoreVariant,
) -> ApiResult<Value> {
    scores::validate_partition(mode, variant)?;
    let row=sqlx::query("SELECT id,username,email,email_verified_at,country,bio,avatar_url,banner_url,accent_color,banner_position_x,banner_position_y,preferred_mode,preferred_variant,preferences,joined_at,is_restricted FROM users WHERE id=$1 AND ($2 OR NOT is_restricted)")
        .bind(id).bind(private).fetch_optional(&state.db).await?.ok_or_else(ApiError::missing)?;
    let roles=sqlx::query_scalar::<_,String>("SELECT r.name FROM roles r JOIN user_roles ur ON ur.role_id=r.id WHERE ur.user_id=$1 ORDER BY r.id").bind(id).fetch_all(&state.db).await?;
    let permissions=sqlx::query_scalar::<_,String>("SELECT DISTINCT unnest(r.permissions) FROM roles r JOIN user_roles ur ON ur.role_id=r.id WHERE ur.user_id=$1").bind(id).fetch_all(&state.db).await?;
    let stats = scores::statistics(state, id, mode, variant).await?;
    let preferences: Preferences =
        serde_json::from_value(row.get::<Value, _>("preferences")).unwrap_or_default();
    let mut value = json!({"id":id,"username":row.get::<String,_>("username"),"country":row.get::<String,_>("country"),"bio":row.get::<String,_>("bio"),"avatar_url":row.get::<Option<String>,_>("avatar_url"),"banner_url":row.get::<Option<String>,_>("banner_url"),"accent_color":row.get::<String,_>("accent_color"),"banner_position_x":row.get::<i32,_>("banner_position_x"),"banner_position_y":row.get::<i32,_>("banner_position_y"),"preferred_mode":row.get::<i16,_>("preferred_mode"),"preferred_variant":row.get::<String,_>("preferred_variant"),"roles":roles,"joined_at":row.get::<chrono::DateTime<Utc>,_>("joined_at"),"pp":stats["pp"],"play_count":stats["play_count"],"accuracy":stats["accuracy"],"statistics":stats});
    // Only roles flagged as badges are public; the rest stay permission plumbing.
    let groups = sqlx::query("SELECT r.id,r.name,r.short_name,r.colour,r.icon FROM roles r JOIN user_roles ur ON ur.role_id=r.id WHERE ur.user_id=$1 AND r.display_badge ORDER BY r.rank DESC,r.id")
        .bind(id).fetch_all(&state.db).await?;
    value["groups"] = json!(groups.into_iter().map(|g| json!({"id":g.get::<i64,_>("id"),"name":g.get::<String,_>("name"),"short_name":g.get::<String,_>("short_name"),"colour":g.get::<Option<String>,_>("colour"),"icon":g.get::<Option<String>,_>("icon")})).collect::<Vec<_>>());
    if private {
        value["email"] = json!(row.get::<String, _>("email"));
        value["email_verified"] = json!(
            row.get::<Option<chrono::DateTime<Utc>>, _>("email_verified_at")
                .is_some()
        );
        value["permissions"] = json!(permissions);
        value["preferences"] = json!(preferences);
        value["locale"] = json!(preferences.locale);
    }
    Ok(value)
}
pub(crate) async fn hash_password(password: String) -> ApiResult<(String, String)> {
    tokio::task::spawn_blocking(move || {
        let argon = Argon2::default();
        let md5 = format!("{:x}", md5::compute(password.as_bytes()));
        let hash = argon
            .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
            .map_err(ApiError::internal)?
            .to_string();
        let legacy = argon
            .hash_password(md5.as_bytes(), &SaltString::generate(&mut OsRng))
            .map_err(ApiError::internal)?
            .to_string();
        Ok((hash, legacy))
    })
    .await
    .map_err(ApiError::internal)?
}
async fn verify(password: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash).ok().is_some_and(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
    })
    .await
    .unwrap_or(false)
}
async fn session(state: &AppState, id: i64, credential: &str) -> ApiResult<Response> {
    let mut tx = state.db.begin().await?;
    // Serialize issuance with credential changes, then recheck the verified
    // snapshot. An old-password verification cannot mint a post-reset session.
    let current = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM users WHERE id=$1 AND password_hash=$2 AND NOT is_restricted FOR UPDATE",
    )
    .bind(id)
    .bind(credential)
    .fetch_optional(&mut *tx)
    .await?;
    if current.is_none() {
        return Err(ApiError::unauthorized());
    }
    let sid = Uuid::new_v4();
    let expiry = Utc::now() + Duration::days(7);
    sqlx::query("INSERT INTO sessions(id,user_id,expires_at) VALUES($1,$2,$3)")
        .bind(sid)
        .bind(id)
        .bind(expiry)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let token = encode(
        &Header::default(),
        &Claims {
            sub: id,
            sid,
            exp: expiry.timestamp() as usize,
        },
        &EncodingKey::from_secret(state.secret.as_bytes()),
    )
    .map_err(ApiError::internal)?;
    let cookie = format!(
        "illya_session={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=604800{}",
        if state.secure_cookie { "; Secure" } else { "" }
    );
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(json!({"user":profile(state,id,true).await?})),
    )
        .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    username: String,
    email: String,
    password: String,
    locale: Option<String>,
}
pub async fn register(
    State(state): State<AppState>,
    Json(input): Json<Registration>,
) -> ApiResult<Response> {
    validate_username(&input.username).map_err(ApiError::bad)?;
    validate_password(&input.password).map_err(ApiError::bad)?;
    let email = crate::account_recovery::normalize_email(&input.email)?;
    let preferences = Preferences {
        locale: input.locale.unwrap_or_else(|| "zh-TW".into()),
        ..Preferences::default()
    };
    preferences.validate().map_err(ApiError::bad)?;
    let (hash, legacy) = hash_password(input.password).await?;
    let mut tx = state.db.begin().await?;
    ensure_username_available(&mut tx, &input.username, None).await?;
    let id:i64=sqlx::query_scalar("INSERT INTO users(username,email,password_hash,password_md5_hash,preferences) VALUES($1,$2,$3,$4,$5) RETURNING id").bind(input.username).bind(email).bind(&hash).bind(legacy).bind(json!(preferences)).fetch_one(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO user_roles(user_id,role_id) SELECT $1,id FROM roles WHERE name='player'",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    crate::account_recovery::enqueue_verification_in_tx(&state, &mut tx, id).await?;
    tx.commit().await?;
    session(&state, id, &hash).await
}
#[derive(Deserialize)]
pub struct Login {
    username: String,
    password: String,
}
pub async fn login(State(state): State<AppState>, Json(input): Json<Login>) -> ApiResult<Response> {
    if input.username.len() > 254 || input.password.len() > 128 {
        return Err(ApiError::unauthorized());
    }
    let row=sqlx::query("SELECT id,password_hash,password_md5_hash,is_restricted FROM users WHERE lower(username)=lower($1) OR lower(email)=lower($1)").bind(input.username.trim()).fetch_optional(&state.db).await?;
    let Some(row) = row else {
        // Spend one password verification on unknown accounts too.
        let _=verify(input.password,"$argon2id$v=19$m=19456,t=2,p=1$MTIzNDU2Nzg5MGFiY2RlZg$HsHQAI/BTc+rAoLRycoM5qNHkmguCeOIW7+NXhszGsE".into()).await;
        return Err(ApiError::unauthorized());
    };
    let mut credential: String = row.get("password_hash");
    let verified = verify(input.password.clone(), credential.clone()).await;
    if row.get::<bool, _>("is_restricted") {
        return Err(ApiError::unauthorized());
    }
    if !verified {
        // Accounts imported from another server only carry the MD5-derived credential that
        // the game client already sends, so accepting it here exposes no new secret.
        let md5 = format!("{:x}", md5::compute(input.password.as_bytes()));
        if !verify(md5, row.get("password_md5_hash")).await {
            return Err(ApiError::unauthorized());
        }
        let (upgraded, _) = hash_password(input.password).await?;
        let changed =
            sqlx::query("UPDATE users SET password_hash=$3 WHERE id=$1 AND password_hash=$2")
                .bind(row.get::<i64, _>("id"))
                .bind(&credential)
                .bind(&upgraded)
                .execute(&state.db)
                .await?
                .rows_affected();
        if changed != 1 {
            return Err(ApiError::unauthorized());
        }
        credential = upgraded;
    }
    session(&state, row.get("id"), &credential).await
}
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Ok(user) = require(&state, &headers).await {
        sqlx::query("DELETE FROM sessions WHERE id=$1")
            .bind(user.session)
            .execute(&state.db)
            .await?;
    }
    let cookie = format!(
        "illya_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}",
        if state.secure_cookie { "; Secure" } else { "" }
    );
    Ok(([(header::SET_COOKIE, cookie)], Json(json!({"ok":true}))).into_response())
}
pub async fn me(State(state): State<AppState>, headers: HeaderMap) -> ApiResult {
    let user = require(&state, &headers).await?;
    Ok(Json(profile(&state, user.id, true).await?))
}
#[derive(Deserialize)]
pub struct StatisticsQuery {
    mode: Option<i16>,
    variant: Option<ScoreVariant>,
}
pub async fn user_profile(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<StatisticsQuery>,
) -> ApiResult {
    Ok(Json(
        profile_in_variant(
            &state,
            id,
            false,
            query.mode.unwrap_or(0),
            query.variant.unwrap_or_default(),
        )
        .await?,
    ))
}
pub async fn preferences(State(state): State<AppState>, headers: HeaderMap) -> ApiResult {
    let user = require(&state, &headers).await?;
    let value: Value = sqlx::query_scalar("SELECT preferences FROM users WHERE id=$1")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(json!(
        serde_json::from_value::<Preferences>(value).unwrap_or_default()
    )))
}
pub async fn update_preferences(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(patch): Json<Value>,
) -> ApiResult {
    let user = require(&state, &headers).await?;
    let object = patch
        .as_object()
        .ok_or_else(|| ApiError::bad("Preferences must be an object."))?;
    if object.get("background_mode").is_some_and(Value::is_null) {
        return Err(ApiError::bad(
            "Choose default, image or color background mode.",
        ));
    }
    let mut tx = state.db.begin().await?;
    let user = mutation_actor(&mut tx, &user, &[], MutationScope::Session).await?;
    let original: Value =
        sqlx::query_scalar("SELECT preferences FROM users WHERE id=$1 FOR UPDATE")
            .bind(user.id)
            .fetch_one(&mut *tx)
            .await?;
    let mut value = json!(serde_json::from_value::<Preferences>(original).unwrap_or_default());
    for (key, v) in object {
        value[key] = v.clone();
    }
    let preferences: Preferences =
        serde_json::from_value(value).map_err(|e| ApiError::bad(e.to_string()))?;
    preferences.validate().map_err(ApiError::bad)?;
    sqlx::query("UPDATE users SET preferences=$2 WHERE id=$1")
        .bind(user.id)
        .bind(json!(preferences))
        .execute(&mut *tx)
        .await?;
    ensure_mutation_actor(&mut tx, &user).await?;
    tx.commit().await?;
    Ok(Json(json!(preferences)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilePatch {
    bio: Option<String>,
    country: Option<String>,
    accent_color: Option<String>,
    banner_position_x: Option<i32>,
    banner_position_y: Option<i32>,
    preferred_mode: Option<i16>,
    preferred_variant: Option<String>,
}
pub async fn update_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ProfilePatch>,
) -> ApiResult {
    let user = require(&state, &headers).await?;
    if input
        .bio
        .as_ref()
        .is_some_and(|v| v.chars().count() > 4000 || v.contains('\0'))
    {
        return Err(ApiError::bad("Biography must be at most 4,000 characters."));
    }
    if input
        .country
        .as_ref()
        .is_some_and(|v| v.len() != 2 || !v.bytes().all(|c| c.is_ascii_uppercase()))
    {
        return Err(ApiError::bad(
            "Country must be a two-letter uppercase country code.",
        ));
    }
    if input
        .accent_color
        .as_ref()
        .is_some_and(|v| !validate_color(v))
    {
        return Err(ApiError::bad("Enter a six-digit hex color."));
    }
    if [input.banner_position_x, input.banner_position_y]
        .into_iter()
        .flatten()
        .any(|position| !(0..=100).contains(&position))
    {
        return Err(ApiError::bad(
            "Banner positions must be integers from 0 to 100.",
        ));
    }
    // The pair is validated together: mania has no Relax partition.
    if input.preferred_mode.is_some() != input.preferred_variant.is_some() {
        return Err(ApiError::bad(
            "Send the preferred mode and variant together.",
        ));
    }
    if let (Some(mode), Some(variant)) = (input.preferred_mode, input.preferred_variant.as_deref())
        && (!(0..=3).contains(&mode)
            || !matches!(variant, "vanilla" | "relax")
            || (mode == 3 && variant == "relax"))
    {
        return Err(ApiError::bad("Choose a valid preferred mode."));
    }
    let mut tx = state.db.begin().await?;
    let user = mutation_actor(&mut tx, &user, &[], MutationScope::Session).await?;
    sqlx::query("UPDATE users SET bio=COALESCE($2,bio),country=COALESCE($3,country),accent_color=COALESCE($4,accent_color),banner_position_x=COALESCE($5,banner_position_x),banner_position_y=COALESCE($6,banner_position_y),preferred_mode=COALESCE($7,preferred_mode),preferred_variant=COALESCE($8,preferred_variant) WHERE id=$1").bind(user.id).bind(input.bio).bind(input.country).bind(input.accent_color).bind(input.banner_position_x).bind(input.banner_position_y).bind(input.preferred_mode).bind(&input.preferred_variant).execute(&mut *tx).await?;
    ensure_mutation_actor(&mut tx, &user).await?;
    tx.commit().await?;
    Ok(Json(profile(&state, user.id, true).await?))
}
pub async fn upload_media(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> ApiResult {
    let user = require(&state, &headers).await?;
    let _permit = state.upload_permit()?;
    let mut kind = None;
    let mut bytes = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad(e.to_string()))?
    {
        match field.name() {
            Some("kind") => {
                kind = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| ApiError::bad(e.to_string()))?,
                )
            }
            Some("file") => {
                let mut field = field;
                let mut buffer = Vec::new();
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|e| ApiError::bad(e.to_string()))?
                {
                    if buffer.len() + chunk.len() > 12 * 1024 * 1024 {
                        return Err(ApiError::bad("Images must be at most 12 MiB."));
                    }
                    buffer.extend_from_slice(&chunk);
                }
                bytes = Some(buffer);
            }
            _ => {}
        }
    }
    let kind = kind.ok_or_else(|| ApiError::bad("Choose avatar, banner or background."))?;
    let max_width = match kind.as_str() {
        "avatar" => 512,
        "banner" => 1920,
        "background" => 2560,
        _ => return Err(ApiError::bad("Unknown image kind.")),
    };
    let bytes = bytes.ok_or_else(|| ApiError::bad("Attach an image in the file field."))?;
    let encoded = tokio::task::spawn_blocking(move || -> ApiResult<Vec<u8>> {
        let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|_| ApiError::bad("Invalid image."))?;
        if !matches!(
            reader.format(),
            Some(image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP)
        ) {
            return Err(ApiError::bad("Use a PNG, JPEG or WebP image."));
        }
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(128 * 1024 * 1024);
        reader.limits(limits);
        let image = reader
            .decode()
            .map_err(|_| ApiError::bad("Image is invalid or too large."))?;
        let image = image.thumbnail(max_width, if max_width == 512 { 512 } else { 2560 });
        let mut output = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut output, image::ImageFormat::WebP)
            .map_err(ApiError::internal)?;
        Ok(output.into_inner())
    })
    .await
    .map_err(ApiError::internal)??;
    let relative = format!("{kind}/{}.webp", Uuid::new_v4());
    tokio::fs::create_dir_all(state.media_root.join(&kind)).await?;
    // Only avatar/banner/background are served. Keep private staging inside
    // the media volume so publication never renames across a mount boundary.
    // Multipart/image work and file writing hold no authorization locks.
    let staging_root = state.media_root.join(".illya-media-staging");
    tokio::fs::create_dir_all(&staging_root).await?;
    let temporary = staging_root.join(format!("{}.part", Uuid::new_v4()));
    let destination = state.media_root.join(&relative);
    let url = format!("/media/{relative}");
    let mut staged = false;
    let mut published = false;
    let mut commit_attempted = false;
    let result = async {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await?;
        staged = true;
        file.write_all(&encoded).await?;
        file.sync_all().await?;
        drop(file);

        let mut tx = state.db.begin().await?;
        let user = mutation_actor(&mut tx, &user, &[], MutationScope::Session).await?;
        match kind.as_str() {
            "avatar" => {
                sqlx::query("UPDATE users SET avatar_url=$2 WHERE id=$1")
                    .bind(user.id)
                    .bind(&url)
                    .execute(&mut *tx)
                    .await?;
            }
            "banner" => {
                sqlx::query("UPDATE users SET banner_url=$2 WHERE id=$1")
                    .bind(user.id)
                    .bind(&url)
                    .execute(&mut *tx)
                    .await?;
            }
            _ => {
                sqlx::query("UPDATE users SET preferences=jsonb_set(jsonb_set(preferences,'{background_url}',to_jsonb($2::text)),'{background_mode}','\"image\"'::jsonb) WHERE id=$1").bind(user.id).bind(&url).execute(&mut *tx).await?;
            }
        }
        ensure_mutation_actor(&mut tx, &user).await?;
        tokio::fs::rename(&temporary, &destination).await?;
        published = true;
        ensure_mutation_actor(&mut tx, &user).await?;
        // A failed COMMIT acknowledgement may follow a successful commit.
        // Retain the published file in that case rather than break its URL.
        commit_attempted = true;
        tx.commit().await?;
        Ok(Json(json!({"url":url,"kind":kind})))
    }
    .await;
    if result.is_err() && !commit_attempted {
        if staged {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        if published {
            let _ = tokio::fs::remove_file(&destination).await;
        }
    }
    result
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileMediaKind {
    Avatar,
    Banner,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileMediaRemoval {
    kind: ProfileMediaKind,
}

pub async fn remove_profile_media(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ProfileMediaRemoval>,
) -> ApiResult {
    let user = require(&state, &headers).await?;
    let mut tx = state.db.begin().await?;
    let user = mutation_actor(&mut tx, &user, &[], MutationScope::Session).await?;
    let update = match input.kind {
        ProfileMediaKind::Avatar => {
            "UPDATE users SET avatar_url=NULL WHERE id=$1 AND NOT is_restricted AND EXISTS(SELECT 1 FROM sessions WHERE id=$2 AND user_id=$1 AND expires_at>clock_timestamp())"
        }
        ProfileMediaKind::Banner => {
            "UPDATE users SET banner_url=NULL WHERE id=$1 AND NOT is_restricted AND EXISTS(SELECT 1 FROM sessions WHERE id=$2 AND user_id=$1 AND expires_at>clock_timestamp())"
        }
    };
    let changed = sqlx::query(update)
        .bind(user.id)
        .bind(user.session)
        .execute(&mut *tx)
        .await?;
    if changed.rows_affected() != 1 {
        return Err(ApiError::unauthorized());
    }
    ensure_mutation_actor(&mut tx, &user).await?;
    tx.commit().await?;
    Ok(Json(json!({"kind":input.kind,"url":null})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordChange {
    current_password: String,
    new_password: String,
}
/// The caller holds the user's mutation fence before revoking login sessions.
/// Keeping one web session supports an authenticated own-account operation.
pub(crate) async fn revoke_login_sessions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: i64,
    keep_web_session: Option<Uuid>,
) -> ApiResult<()> {
    sqlx::query("DELETE FROM sessions WHERE user_id=$1 AND ($2::uuid IS NULL OR id<>$2)")
        .bind(user_id)
        .bind(keep_web_session)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM bancho_sessions WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM lazer_sessions WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// Credential changes invalidate both logins and pending account recovery.
/// Plain session logout leaves recovery links and their mail delivery intact.
pub(crate) async fn revoke_sessions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: i64,
    keep_web_session: Option<Uuid>,
) -> ApiResult<()> {
    revoke_login_sessions(tx, user_id, keep_web_session).await?;
    sqlx::query("UPDATE account_recovery_tokens SET consumed_at=COALESCE(consumed_at,now()) WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE mail_jobs SET status='canceled',encrypted_payload=NULL,lease_until=NULL,lease_id=NULL,last_error_code=NULL WHERE user_id=$1 AND status IN ('pending','sending','failed')")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
pub async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PasswordChange>,
) -> ApiResult {
    let user = require(&state, &headers).await?;
    validate_password(&input.new_password).map_err(ApiError::bad)?;
    if input.current_password.len() > 128 {
        return Err(ApiError::unauthorized());
    }
    let verified: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id=$1")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    if !verify(input.current_password, verified.clone()).await {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "invalid_password",
            "The current password is incorrect.".into(),
        ));
    }
    let (hash, legacy) = hash_password(input.new_password).await?;
    let mut tx = state.db.begin().await?;
    let user = mutation_actor(&mut tx, &user, &[], MutationScope::Session).await?;
    let current: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id=$1")
        .bind(user.id)
        .fetch_one(&mut *tx)
        .await?;
    if current != verified {
        return Err(ApiError::unauthorized());
    }
    sqlx::query("UPDATE users SET password_hash=$2,password_md5_hash=$3 WHERE id=$1")
        .bind(user.id)
        .bind(hash)
        .bind(legacy)
        .execute(&mut *tx)
        .await?;
    revoke_sessions(&mut tx, user.id, Some(user.session)).await?;
    sqlx::query("INSERT INTO audit_events(actor_id,action,target_type,target_id,details) VALUES($1,'users.password_change','user',$2,'{\"other_sessions_revoked\":true}')").bind(user.id).bind(user.id.to_string()).execute(&mut *tx).await?;
    ensure_mutation_actor(&mut tx, &user).await?;
    tx.commit().await?;
    Ok(Json(json!({"ok":true})))
}
pub async fn bootstrap(state: &AppState) -> ApiResult<()> {
    let Ok(username) = std::env::var("BOOTSTRAP_ADMIN_USERNAME") else {
        return Ok(());
    };
    if username.trim().is_empty() {
        return Ok(());
    }
    let password = std::env::var("BOOTSTRAP_ADMIN_PASSWORD")
        .map_err(|_| ApiError::bad("Set BOOTSTRAP_ADMIN_PASSWORD."))?;
    let email = std::env::var("BOOTSTRAP_ADMIN_EMAIL")
        .map_err(|_| ApiError::bad("Set BOOTSTRAP_ADMIN_EMAIL."))?;
    let email = crate::account_recovery::normalize_email(&email)?;
    validate_username(&username).map_err(ApiError::bad)?;
    validate_password(&password).map_err(ApiError::bad)?;
    let existing =
        sqlx::query("SELECT id,password_hash FROM users WHERE lower(username)=lower($1)")
            .bind(&username)
            .fetch_optional(&state.db)
            .await?;
    let created = existing.is_none();
    let id = if let Some(row) = existing {
        if !verify(password, row.get("password_hash")).await {
            return Err(ApiError::bad(
                "Bootstrap administrator credentials do not match the existing account.",
            ));
        }
        row.get::<i64, _>("id")
    } else {
        let (hash, legacy) = hash_password(password).await?;
        sqlx::query_scalar("INSERT INTO users(username,email,password_hash,password_md5_hash) VALUES($1,$2,$3,$4) RETURNING id").bind(username).bind(email.to_lowercase()).bind(hash).bind(legacy).fetch_one(&state.db).await?
    };
    sqlx::query("INSERT INTO user_roles(user_id,role_id) SELECT $1,id FROM roles WHERE name IN ('administrator','player') ON CONFLICT DO NOTHING").bind(id).execute(&state.db).await?;
    // The account that first sets up a server owns it; later transfers go through the CLI.
    if created {
        sqlx::query("INSERT INTO user_roles(user_id,role_id) SELECT $1,id FROM roles WHERE name='super_user' AND NOT EXISTS(SELECT 1 FROM user_roles ur WHERE ur.role_id=roles.id)").bind(id).execute(&state.db).await?;
    }
    Ok(())
}

/// A player may rename once per this many days; staff renames do not use the allowance.
const USERNAME_COOLDOWN_DAYS: i64 = 30;
/// A released name stays reserved for its previous owner this long.
const USERNAME_RESERVED_DAYS: i64 = 90;

/// Reject a name another account holds, or recently released and still reserved for someone else.
pub(crate) async fn ensure_username_available(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    name: &str,
    for_user: Option<i64>,
) -> ApiResult<()> {
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE lower(username)=lower($1) AND id IS DISTINCT FROM $2)",
    )
    .bind(name)
    .bind(for_user)
    .fetch_one(&mut **tx)
    .await?;
    if taken {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "username_taken",
            "That username is already taken.".into(),
        ));
    }
    let reserved: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM username_history WHERE lower(old_username)=lower($1) AND user_id IS DISTINCT FROM $2 AND changed_at>now()-make_interval(days=>$3::int))",
    )
    .bind(name)
    .bind(for_user)
    .bind(USERNAME_RESERVED_DAYS as i32)
    .fetch_one(&mut **tx)
    .await?;
    if reserved {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "username_reserved",
            "That username was recently released and is reserved for its previous owner.".into(),
        ));
    }
    Ok(())
}

/// Rename inside the caller's transaction. Stable sessions are closed so connected clients sign in
/// again and show the new name; scores, chat and relationships follow the account id.
pub(crate) async fn rename_user_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    target: i64,
    new_name: &str,
    actor: i64,
    enforce_cooldown: bool,
) -> ApiResult<()> {
    validate_username(new_name).map_err(ApiError::bad)?;
    let row = sqlx::query("SELECT username,username_changed_at,username_changed_at>now()-make_interval(days=>$2::int) AS cooling FROM users WHERE id=$1 FOR UPDATE")
        .bind(target)
        .bind(USERNAME_COOLDOWN_DAYS as i32)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(ApiError::missing)?;
    let old: String = row.get("username");
    if old == new_name {
        return Err(ApiError::bad("Choose a different username."));
    }
    if enforce_cooldown && row.get::<Option<bool>, _>("cooling").unwrap_or(false) {
        let until: chrono::DateTime<Utc> = row
            .get::<chrono::DateTime<Utc>, _>("username_changed_at")
            + chrono::Duration::days(USERNAME_COOLDOWN_DAYS);
        return Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "username_cooldown",
            format!(
                "You can change your username again on {}.",
                until.format("%Y-%m-%d")
            ),
        ));
    }
    ensure_username_available(tx, new_name, Some(target)).await?;
    sqlx::query(
        "UPDATE users SET username=$2,username_changed_at=CASE WHEN $3 THEN now() ELSE username_changed_at END WHERE id=$1",
    )
    .bind(target)
    .bind(new_name)
    .bind(enforce_cooldown)
    .execute(&mut **tx)
    .await?;
    sqlx::query("INSERT INTO username_history(user_id,old_username,new_username,changed_by) VALUES($1,$2,$3,$4)")
        .bind(target)
        .bind(&old)
        .bind(new_name)
        .bind(actor)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM bancho_sessions WHERE user_id=$1")
        .bind(target)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO audit_events(actor_id,action,target_type,target_id,details) VALUES($1,'users.rename','user',$2,$3)")
        .bind(actor)
        .bind(target.to_string())
        .bind(json!({"old":old,"new":new_name,"staff":actor!=target}))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsernameChange {
    username: String,
    current_password: String,
}
pub async fn change_username(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<UsernameChange>,
) -> ApiResult {
    let user = require(&state, &headers).await?;
    validate_username(&input.username).map_err(ApiError::bad)?;
    if input.current_password.len() > 128 {
        return Err(ApiError::unauthorized());
    }
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id=$1")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    if !verify(input.current_password, hash).await {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "invalid_password",
            "The current password is incorrect.".into(),
        ));
    }
    let mut tx = state.db.begin().await?;
    let user = mutation_actor(&mut tx, &user, &[], MutationScope::Session).await?;
    rename_user_in_tx(&mut tx, user.id, &input.username, user.id, true).await?;
    ensure_mutation_actor(&mut tx, &user).await?;
    tx.commit().await?;
    Ok(Json(profile(&state, user.id, true).await?))
}

#[cfg(test)]
#[path = "auth_mutation_tests.rs"]
mod mutation_tests;

#[cfg(test)]
#[path = "auth_background_preferences_tests.rs"]
mod background_preferences_tests;
