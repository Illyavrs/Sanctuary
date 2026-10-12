//! Player-owned replay claims. Structural validation is not gameplay verification.
//! Save the replay and score together; only scores.moderate can publish the claim.
use crate::{ApiError, ApiResult, AppState, auth, beatmap_availability as availability, scores};
use axum::{
    Json,
    extract::{Multipart, State},
    http::{HeaderMap, StatusCode},
};
use chrono::{DateTime, Utc};
use illya_domain::HitCounts;
use resonance_protocol::{
    lazer::SoloScoreSubmission,
    lazer_replay::{self, ExtendedReplay},
    replay::{LegacyReplay, MAX_REPLAY_BYTES},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

fn invalid(code: &'static str) -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        code,
        "The replay cannot be submitted for review.".into(),
    )
}

struct ParsedReplay {
    input: scores::Submission,
    username: String,
    native_user: Option<i64>,
    checksum: String,
    played_at: DateTime<Utc>,
    fingerprint: String,
    client: &'static str,
}

// Passed extended replays contain full map maxima. Use the pinned score
// processor's accuracy weights; failed partial summaries use legacy counters.
fn native_accuracy(native: &SoloScoreSubmission, fallback: f64) -> ApiResult<f64> {
    if !native.passed {
        return Ok(fallback / 100.0);
    }
    let weight = |name: &str| -> u64 {
        match (native.ruleset_id, name) {
            (2, "great" | "large_tick_hit" | "small_tick_hit") => 300,
            (3, "perfect") => 305,
            (1, "ok") => 150,
            (_, "great" | "perfect") => 300,
            (_, "good") => 200,
            (_, "ok") => 100,
            (_, "meh") => 50,
            (_, "large_tick_hit") => 30,
            (_, "small_tick_hit") => 10,
            (_, "slider_tail_hit") => 150,
            _ => 0,
        }
    };
    let sum = |counts: &std::collections::BTreeMap<String, u32>| {
        counts
            .iter()
            .map(|(name, count)| weight(name) * u64::from(*count))
            .sum::<u64>()
    };
    let maximum = sum(&native.maximum_statistics);
    let actual = sum(&native.statistics);
    if maximum == 0 || actual > maximum {
        return Err(invalid("replay_invalid"));
    }
    Ok(actual as f64 / maximum as f64)
}

fn parse(bytes: &[u8], passed: bool) -> ApiResult<ParsedReplay> {
    let version = bytes
        .get(1..5)
        .and_then(|v| v.try_into().ok())
        .map(i32::from_le_bytes)
        .ok_or_else(|| invalid("replay_invalid"))?;
    let extended = if (30_000_001..31_000_000).contains(&version) {
        Some(ExtendedReplay::parse_for_review(bytes).map_err(|_| invalid("replay_invalid"))?)
    } else {
        None
    };
    let legacy = if let Some(replay) = &extended {
        &replay.legacy
    } else {
        // Retain a separate borrowed container for legacy exports.
        return parse_legacy(bytes, passed);
    };
    let metadata = &extended.as_ref().unwrap().metadata;
    let mut native = SoloScoreSubmission {
        ruleset_id: legacy.mode,
        passed: metadata.rank.as_deref() != Some("F"),
        total_score: legacy.total_score as u64,
        total_score_without_mods: metadata
            .total_score_without_mods
            .or_else(|| (legacy.total_score == 0).then_some(0))
            .ok_or_else(|| invalid("replay_invalid"))?,
        accuracy: 0.0,
        max_combo: u32::from(legacy.max_combo),
        rank: metadata
            .rank
            .clone()
            .ok_or_else(|| invalid("replay_invalid"))?,
        mods: metadata.mods.clone(),
        statistics: metadata.statistics.clone(),
        maximum_statistics: metadata.maximum_statistics.clone(),
        pauses: metadata.pauses.clone(),
    };
    let counts: HitCounts = serde_json::from_value(native.legacy_hit_counts())
        .map_err(|_| invalid("replay_invalid"))?;
    native.accuracy = native_accuracy(&native, scores::accuracy(i16::from(legacy.mode), &counts)?)?;
    native.validate().map_err(|_| invalid("replay_invalid"))?;
    let validated =
        illya_domain::score_mods::ValidatedMods::parse(legacy.mode, &json!(native.mods))
            .map_err(|_| invalid("replay_invalid"))?;
    if validated.legacy_bits != legacy.mods {
        return Err(invalid("replay_invalid"));
    }
    extended
        .as_ref()
        .unwrap()
        .matches(&lazer_replay::ReplayScore {
            online_id: metadata.online_id,
            user_id: metadata.user_id,
            username: legacy.player_name,
            beatmap_md5: legacy.beatmap_md5,
            client_version: "",
            mods_bits: legacy.mods,
            timestamp_ticks: legacy.timestamp_ticks,
            submission: &native,
        })
        .map_err(|_| invalid("replay_invalid"))?;
    let mut parsed = from_legacy(legacy, native.passed)?;
    parsed.native_user = (metadata.user_id > 0).then_some(metadata.user_id);
    parsed.client = "lazer";
    parsed.input.hit_counts = counts;
    parsed.input.accuracy = native.accuracy * 100.0;
    parsed.input.api_mods = Some(json!(native.mods));
    parsed.input.native_submission = Some(native);
    Ok(parsed)
}

fn parse_legacy(bytes: &[u8], passed: bool) -> ApiResult<ParsedReplay> {
    let replay = LegacyReplay::parse(bytes).map_err(|_| invalid("replay_invalid"))?;
    from_legacy(&replay, passed)
}

fn from_legacy(replay: &LegacyReplay<'_>, passed: bool) -> ApiResult<ParsedReplay> {
    let frames = lazer_replay::decompress(replay.compressed_frames, lazer_replay::MAX_FRAME_TEXT)
        .map_err(|_| invalid("replay_invalid"))?;
    lazer_replay::validate_frame_text(&frames).map_err(|_| invalid("replay_invalid"))?;
    if replay.timestamp_ticks > 3_155_378_975_999_999_999 {
        return Err(invalid("replay_invalid"));
    }
    let ticks = replay
        .timestamp_ticks
        .checked_sub(621_355_968_000_000_000)
        .filter(|v| *v >= 0)
        .ok_or_else(|| invalid("replay_invalid"))?;
    let played_at = DateTime::from_timestamp(ticks / 10_000_000, (ticks % 10_000_000) as u32 * 100)
        .ok_or_else(|| invalid("replay_invalid"))?;
    let [n300, n100, n50, geki, katu, misses] = replay.hit_counts.map(u32::from);
    let counts = HitCounts {
        n300,
        n100,
        n50,
        geki,
        katu,
        misses,
        ..Default::default()
    };
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&json!({
                "checksum":replay.beatmap_md5.to_ascii_lowercase(), "mode":replay.mode,
                "score":replay.total_score,"combo":replay.max_combo,"mods":replay.mods,
                "counts":replay.hit_counts,"ticks":replay.timestamp_ticks,
                "frames":format!("{:x}",Sha256::digest(&frames)),
            }))
            .map_err(ApiError::internal)?
        )
    );
    Ok(ParsedReplay {
        username: replay.player_name.into(),
        native_user: None,
        checksum: replay.beatmap_md5.to_ascii_lowercase(),
        played_at,
        fingerprint,
        client: "stable",
        input: scores::Submission {
            user_id: None,
            beatmap_id: 0,
            mode: i16::from(replay.mode),
            score: i64::from(replay.total_score),
            accuracy: scores::accuracy(i16::from(replay.mode), &counts)?,
            max_combo: u32::from(replay.max_combo),
            mods: replay.mods,
            hit_counts: counts,
            api_mods: None,
            native_submission: None,
            passed,
            client: Some("stable".into()),
            ingestion_key: None,
            client_beatmap_md5: Some(replay.beatmap_md5.to_ascii_lowercase()),
            play_seconds: None,
        },
    })
}

async fn retained(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: i64,
    key: &str,
    digest: &str,
) -> ApiResult<Option<Value>> {
    sqlx::query_scalar("SELECT jsonb_build_object('id',id,'verification',verification,'has_replay',replay_path IS NOT NULL,'idempotent',true) FROM scores WHERE user_id=$1 AND (ingestion_key=$2 OR replay_sha256=$3) ORDER BY id LIMIT 1")
        .bind(user).bind(key).bind(digest).fetch_optional(&mut **tx).await.map_err(ApiError::from)
}

pub async fn submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> ApiResult {
    let user = auth::require(&state, &headers).await?;
    let _permit = state.upload_permit()?;
    let mut bytes = None;
    let mut passed = None;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|_| invalid("replay_file"))?
    {
        match field.name() {
            Some("file") if bytes.is_none() => {
                if !field
                    .file_name()
                    .is_some_and(|name| name.to_ascii_lowercase().ends_with(".osr"))
                {
                    return Err(invalid("replay_file"));
                }
                let mut file = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(|_| invalid("replay_file"))? {
                    if file.len().saturating_add(chunk.len()) > MAX_REPLAY_BYTES {
                        return Err(ApiError(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "replay_too_large",
                            "Replay exceeds 16 MiB.".into(),
                        ));
                    }
                    file.extend_from_slice(&chunk);
                }
                if file.is_empty() {
                    return Err(invalid("replay_file"));
                }
                bytes = Some(file);
            }
            Some("passed") if passed.is_none() => {
                let mut value = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(|_| invalid("replay_file"))? {
                    if value.len().saturating_add(chunk.len()) > 5 {
                        return Err(invalid("replay_file"));
                    }
                    value.extend_from_slice(&chunk);
                }
                passed = Some(match value.as_slice() {
                    b"true" => true,
                    b"false" => false,
                    _ => return Err(invalid("replay_file")),
                });
            }
            _ => return Err(invalid("replay_file")),
        }
    }
    let bytes = bytes.ok_or_else(|| invalid("replay_file"))?;
    let passed = passed.ok_or_else(|| invalid("replay_file"))?;
    let (bytes, mut parsed) = tokio::task::spawn_blocking(move || {
        let parsed = parse(&bytes, passed)?;
        Ok::<_, ApiError>((bytes, parsed))
    })
    .await
    .map_err(ApiError::internal)??;
    let sha = format!("{:x}", Sha256::digest(&bytes));
    let key = format!("replay:{}:{}", user.id, parsed.fingerprint);
    let username: String = sqlx::query_scalar("SELECT username FROM users WHERE id=$1")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    if parsed.username != username || parsed.native_user.is_some_and(|id| id != user.id) {
        return Err(invalid("replay_owner"));
    }
    {
        let mut tx = state.db.begin().await?;
        let actor = auth::mutation_actor(&mut tx, &user, &[], auth::MutationScope::Session).await?;
        if let Some(existing) = retained(&mut tx, actor.id, &key, &sha).await? {
            tx.commit().await?;
            return Ok(Json(existing));
        }
    }
    let maps = sqlx::query("SELECT id,osu_path,mode,native_set_id FROM active_beatmaps WHERE illya_beatmap_checksum_matches(id,$1) ORDER BY id LIMIT 2")
        .bind(&parsed.checksum).fetch_all(&state.db).await?;
    if maps.len() != 1 {
        return Err(invalid("replay_map"));
    }
    let map = &maps[0];
    let map_id: i64 = map.get("id");
    let set_id: i64 = map.get("native_set_id");
    let mode: i16 = map.get("mode");
    if mode != 0 && mode != parsed.input.mode {
        return Err(invalid("replay_map"));
    }
    let path: String = map.get("osu_path");
    if std::path::Path::new(&path).is_absolute() || path.contains("..") {
        return Err(invalid("replay_map"));
    }
    let map_bytes = tokio::fs::read(state.maps_root.join(path))
        .await
        .map_err(|_| invalid("replay_map"))?;
    parsed.input.beatmap_id = map_id;
    parsed.input.client = Some(parsed.client.into());
    let (variant, pp_eligible) = scores::mod_partition(&parsed.input, parsed.client)?;
    let input = parsed.input.clone();
    let lazer = parsed.client == "lazer";
    let calculated =
        tokio::task::spawn_blocking(move || scores::calculate_score(&map_bytes, &input, lazer))
            .await
            .map_err(ApiError::internal)??;
    let file_name = format!("replay-{}.osr", Uuid::new_v4());
    let temporary = state.replay_root.join(format!("{file_name}.tmp"));
    let destination = state.replay_root.join(&file_name);
    let mut owned = false;
    let mut published = false;
    let mut commit_attempted = false;
    let outcome: ApiResult = async {
        let mut tx = state.db.begin().await?;
        let actor = auth::mutation_actor(&mut tx,&user,&[],auth::MutationScope::Session).await?;
        let current_name: String = sqlx::query_scalar("SELECT username FROM users WHERE id=$1").bind(actor.id).fetch_one(&mut *tx).await?;
        if parsed.username != current_name { return Err(invalid("replay_owner")); }
        if let Some(existing) = retained(&mut tx,actor.id,&key,&sha).await? { tx.commit().await?; return Ok(Json(existing)); }
        let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM scores WHERE user_id=$1 AND submission_origin='replay' AND verification='pending'").bind(actor.id).fetch_one(&mut *tx).await?;
        if pending >= 20 { return Err(ApiError(StatusCode::TOO_MANY_REQUESTS,"replay_review_limit","Wait for your pending replay submissions to be reviewed.".into())); }
        availability::lock_active_set(&mut tx,set_id).await?;
        let checksum_matches: bool = sqlx::query_scalar("SELECT illya_beatmap_checksum_matches(id,$2) FROM beatmaps WHERE id=$1 FOR SHARE").bind(map_id).bind(&parsed.checksum).fetch_one(&mut *tx).await?;
        if !checksum_matches { return Err(invalid("replay_map")); }
        let mut file = tokio::fs::OpenOptions::new().create_new(true).write(true).open(&temporary).await?;
        owned = true;
        file.write_all(&bytes).await?; file.sync_all().await?; drop(file);
        tokio::fs::rename(&temporary,&destination).await?; published = true;
        let s = &parsed.input; let scales = &calculated.scales;
        let id: i64 = sqlx::query_scalar("INSERT INTO scores(user_id,beatmap_id,mode,score,accuracy,max_combo,mods,pp,passed,hit_counts,client,ingestion_key,api_mods,pp_eligible,standardised_score,standardised_without_mods,classic_score,native_submission,native_basic_maximum,score_scale_status,score_scale_version,score_scale_reason,legacy_attributes,score_variant,pp_calculation_version,client_beatmap_md5,replay_path,replay_sha256,replay_format,replay_complete,verification,submission_origin,replay_played_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25,$26,$27,$28,$29,NULL,'pending','replay',$30) RETURNING id")
            .bind(actor.id).bind(map_id).bind(s.mode).bind(s.score).bind(s.accuracy).bind(s.max_combo as i32).bind(i64::from(s.mods)).bind(calculated.pp).bind(s.passed).bind(json!(s.hit_counts)).bind(parsed.client).bind(&key).bind(&s.api_mods).bind(pp_eligible)
            .bind(scales.standardised_score).bind(scales.standardised_without_mods).bind(scales.classic_score).bind(&scales.native_submission).bind(scales.native_basic_maximum).bind(scales.status).bind(&scales.version).bind(&scales.reason).bind(&scales.legacy_attributes).bind(variant.as_str()).bind(scores::pp_version(variant)).bind(&parsed.checksum).bind(&file_name).bind(&sha).bind(if lazer {"lazer"} else {"legacy"}).bind(parsed.played_at).fetch_one(&mut *tx).await?;
        sqlx::query("INSERT INTO audit_events(actor_id,action,target_type,target_id,details) VALUES($1,'scores.replay_submit','score',$2,$3)")
            .bind(actor.id).bind(id.to_string()).bind(json!({"client":parsed.client,"sha256":sha,"metadata_matches":true,"gameplay_verified":false,"played_at":parsed.played_at})).execute(&mut *tx).await?;
        auth::ensure_mutation_actor(&mut tx,&actor).await?;
        commit_attempted = true;
        if let Err(error) = tx.commit().await { commit_attempted = !crate::maps::commit_definitely_rolled_back(&error); return Err(error.into()); }
        Ok(Json(json!({"id":id,"verification":"pending","has_replay":true,"submission_origin":"replay","idempotent":false})))
    }.await;
    if owned {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    if published && !commit_attempted {
        let _ = tokio::fs::remove_file(&destination).await;
    }
    outcome
}

#[cfg(test)]
#[path = "score_resubmission_tests.rs"]
mod tests;
