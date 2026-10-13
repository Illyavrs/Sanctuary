//! Administrator-owned frame artwork is bounded, decoded, and published as an
//! immutable local PNG. Browser supplied names/paths never reach the filesystem.
use crate::{ApiError, ApiResult, AppState, auth, maps, rewards};
use axum::{
    Json,
    extract::{Multipart, Path, State},
    http::HeaderMap,
};
use image::{DynamicImage, ImageDecoder, ImageFormat};
use std::io::Cursor;
use uuid::Uuid;

const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_SIDE: u32 = 1024;

fn frame_dimensions(width: u32, height: u32) -> ApiResult<()> {
    if width != height || !(128..=MAX_SIDE).contains(&width) {
        return Err(ApiError::bad(
            "Frame images must be square and 128–1024 pixels on each side.",
        ));
    }
    Ok(())
}

// Extended WebP has separate canvas and bitstream dimensions. The image decoder
// can allocate a lossy frame before checking that they agree, so inspect every
// declared frame size and bounded RIFF chunk before invoking its pixel decoder.
fn webp_preflight(bytes: &[u8]) -> ApiResult<()> {
    let invalid = || ApiError::bad("Invalid WebP container.");
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err(invalid());
    }
    let end = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize + 8;
    if end != bytes.len() {
        return Err(invalid());
    }
    let mut position = 12;
    let mut dimensions = None;
    while position < end {
        let header = bytes.get(position..position + 8).ok_or_else(invalid)?;
        let length = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let start = position + 8;
        let stop = start
            .checked_add(length)
            .filter(|stop| *stop <= end)
            .ok_or_else(invalid)?;
        position = stop
            .checked_add(length & 1)
            .filter(|next| *next <= end)
            .ok_or_else(invalid)?;
        let chunk = &bytes[start..stop];
        let size = match &header[..4] {
            b"VP8X" if length == 10 => {
                if chunk[0] & 2 != 0 {
                    return Err(ApiError::bad(
                        "Animated images are not supported for avatar frames.",
                    ));
                }
                let value = |offset| {
                    u32::from(chunk[offset])
                        | (u32::from(chunk[offset + 1]) << 8)
                        | (u32::from(chunk[offset + 2]) << 16)
                };
                Some((value(4) + 1, value(7) + 1))
            }
            b"VP8 " if length >= 10 && &chunk[3..6] == b"\x9d\x01\x2a" => Some((
                u32::from(u16::from_le_bytes([chunk[6], chunk[7]]) & 0x3fff),
                u32::from(u16::from_le_bytes([chunk[8], chunk[9]]) & 0x3fff),
            )),
            b"VP8L" if length >= 5 && chunk[0] == 0x2f => {
                let bits = u32::from_le_bytes(chunk[1..5].try_into().unwrap());
                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            b"ANIM" | b"ANMF" => {
                return Err(ApiError::bad(
                    "Animated images are not supported for avatar frames.",
                ));
            }
            b"VP8X" | b"VP8 " | b"VP8L" => return Err(invalid()),
            _ => None,
        };
        if let Some(size) = size {
            frame_dimensions(size.0, size.1)?;
            if dimensions.is_some_and(|previous| previous != size) {
                return Err(ApiError::bad(
                    "WebP canvas and image dimensions must match.",
                ));
            }
            dimensions = Some(size);
        }
    }
    Ok(())
}

fn rgba_frame(decoder: impl ImageDecoder) -> ApiResult<DynamicImage> {
    let (width, height) = decoder.dimensions();
    frame_dimensions(width, height)?;
    let image = DynamicImage::from_decoder(decoder)
        .map_err(|_| ApiError::bad("The frame image is invalid or too large."))?
        .to_rgba8();
    let mut clear = false;
    let mut visible = false;
    for pixel in image.pixels() {
        clear |= pixel[3] == 0;
        visible |= pixel[3] > 0;
    }
    if !clear || !visible {
        return Err(ApiError::bad(
            "Use a transparent image with visible artwork and a clear opening for the avatar.",
        ));
    }
    Ok(DynamicImage::ImageRgba8(image))
}

fn canonical_png(bytes: Vec<u8>) -> ApiResult<Vec<u8>> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return Err(ApiError::bad("Frame images must be at most 2 MiB."));
    }
    let format = image::guess_format(&bytes)
        .map_err(|_| ApiError::bad("Use a static transparent PNG or WebP image."))?;
    let image = match format {
        ImageFormat::Png => {
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(MAX_SIDE);
            limits.max_image_height = Some(MAX_SIDE);
            limits.max_alloc = Some(16 * 1024 * 1024);
            let decoder = image::codecs::png::PngDecoder::with_limits(Cursor::new(bytes), limits)
                .map_err(|_| ApiError::bad("The PNG image is invalid or too large."))?;
            if decoder
                .is_apng()
                .map_err(|_| ApiError::bad("Invalid PNG image."))?
            {
                return Err(ApiError::bad(
                    "Animated images are not supported for avatar frames.",
                ));
            }
            rgba_frame(decoder)?
        }
        ImageFormat::WebP => {
            webp_preflight(&bytes)?;
            // The container parser reads at most the bounded upload. Validate
            // dimensions before from_decoder allocates a pixel buffer.
            let decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
                .map_err(|_| ApiError::bad("Invalid WebP image."))?;
            if decoder.has_animation() {
                return Err(ApiError::bad(
                    "Animated images are not supported for avatar frames.",
                ));
            }
            rgba_frame(decoder)?
        }
        _ => return Err(ApiError::bad("Use a static transparent PNG or WebP image.")),
    };
    let mut output = Cursor::new(Vec::new());
    image
        .write_to(&mut output, ImageFormat::Png)
        .map_err(ApiError::internal)?;
    Ok(output.into_inner())
}

pub async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    mut multipart: Multipart,
) -> ApiResult {
    let snapshot = auth::require(&state, &headers).await?;
    snapshot.require("rewards.manage")?;
    rewards::positive(id)?;
    let permit = state.upload_permit()?;
    let mut revision = None;
    let mut bytes = None;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|_| ApiError::bad("Invalid multipart upload."))?
    {
        let name = field.name().unwrap_or("").to_owned();
        let limit = match name.as_str() {
            "revision" if revision.is_none() => 32,
            "file" if bytes.is_none() => MAX_BYTES,
            _ => {
                return Err(ApiError::bad(
                    "Upload exactly one revision and one file field.",
                ));
            }
        };
        let mut buffer = Vec::new();
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|_| ApiError::bad("Invalid multipart upload."))?
        {
            if chunk.len() > limit.saturating_sub(buffer.len()) {
                return Err(ApiError::bad(if name == "file" {
                    "Frame images must be at most 2 MiB."
                } else {
                    "Invalid reward revision."
                }));
            }
            buffer.extend_from_slice(&chunk);
        }
        if name == "revision" {
            let value = std::str::from_utf8(&buffer)
                .ok()
                .filter(|value| {
                    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
                })
                .and_then(|value| value.parse::<i64>().ok())
                .ok_or_else(|| ApiError::bad("Invalid reward revision."))?;
            rewards::positive(value)?;
            revision = Some(value);
        } else {
            bytes = Some(buffer);
        }
    }
    let revision = revision.ok_or_else(|| ApiError::bad("Include the reward revision."))?;
    let bytes = bytes.ok_or_else(|| ApiError::bad("Attach an image in the file field."))?;
    // Expensive image parsing holds no session, role, or catalog locks. Recheck
    // all authorization after it finishes and after any catalog lock wait.
    // Keep the permit in the worker until decoding finishes, even if the HTTP
    // request disconnects while a non-cancellable blocking task is running.
    let (encoded, _permit) = tokio::task::spawn_blocking(move || (canonical_png(bytes), permit))
        .await
        .map_err(ApiError::internal)?;
    let encoded = encoded?;
    let relative = format!("reward-frames/{}.png", Uuid::new_v4());
    let url = format!("/media/{relative}");
    let destination = state.media_root.join(&relative);
    let staging_root = state.media_root.join(".illya-media-staging");
    tokio::fs::create_dir_all(&staging_root).await?;
    tokio::fs::create_dir_all(state.media_root.join("reward-frames")).await?;
    let temporary = staging_root.join(format!("{}.part", Uuid::new_v4()));
    let mut staged = false;
    let mut published = false;
    let mut retain_published = false;
    let result = async {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new().write(true).create_new(true).open(&temporary).await?;
        staged = true;
        file.write_all(&encoded).await?;
        file.sync_all().await?;
        drop(file);

        let mut tx = state.db.begin().await?;
        let actor = auth::mutation_actor(&mut tx, &snapshot, &[], auth::MutationScope::Permissions).await?;
        actor.require("rewards.manage")?;
        let (actual, kind): (i64, String) = sqlx::query_as("SELECT revision,kind FROM reward_catalog WHERE id=$1 FOR UPDATE")
            .bind(id).fetch_optional(&mut *tx).await?.ok_or_else(ApiError::missing)?;
        auth::ensure_mutation_actor(&mut tx, &actor).await?;
        rewards::revision_matches(revision, actual)?;
        if kind != "avatar_frame" {
            return Err(ApiError::bad("Only avatar frame rewards accept an image."));
        }
        let before = rewards::admin_record(&mut tx, id).await?;
        let next = actual.checked_add(1).ok_or_else(|| ApiError::bad("The reward revision is exhausted."))?;
        sqlx::query("UPDATE reward_catalog SET style='image',image_url=$2,revision=$3,updated_at=clock_timestamp() WHERE id=$1")
            .bind(id).bind(&url).bind(next).execute(&mut *tx).await?;
        let item = rewards::admin_record(&mut tx, id).await?;
        rewards::audit_in(&mut tx, actor.id, "rewards.upload_image", "reward", id,
            serde_json::json!({"before":before,"after":item})).await?;
        auth::ensure_mutation_actor(&mut tx, &actor).await?;
        tokio::fs::rename(&temporary, &destination).await?;
        published = true;
        auth::ensure_mutation_actor(&mut tx, &actor).await?;
        // Preserve media after an uncertain COMMIT response; a transport error
        // may follow a successful commit. Definite PG rollback is safe to clean.
        match tx.commit().await {
            Ok(()) => Ok(Json(item)),
            Err(error) => {
                retain_published = !maps::commit_definitely_rolled_back(&error);
                Err(ApiError::from(error))
            }
        }
    }.await;
    if result.is_err() {
        if staged {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        if published && !retain_published {
            let _ = tokio::fs::remove_file(&destination).await;
        }
    }
    result
}

#[cfg(test)]
#[path = "reward_frame_media_tests.rs"]
mod tests;
