//! Canonical local downloads are immutable derivatives of retained uploads.
//! Only the two Metadata identity fields change; stored originals and history
//! keep their original checksums. A complete derived package publishes aliases
//! atomically behind the canonical set's availability fence.
use crate::{ApiError, ApiResult, AppState, beatmap_availability, maps};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{
    collections::HashSet,
    io::{Cursor, Read, Write},
    path::{Component, Path},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

const MAX_OSU: usize = 8 * 1024 * 1024;
const MAX_ARCHIVE: usize = 64 * 1024 * 1024;
const MAX_EXPANDED: usize = 256 * 1024 * 1024;

pub(crate) struct DistributedFile {
    pub(crate) bytes: Vec<u8>,
    pub(crate) md5: Option<String>,
    pub(crate) sha256: String,
}

fn invalid() -> ApiError {
    ApiError::internal("Stored local beatmap content cannot be distributed safely.")
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Keep every unrelated byte, including BOM, CRLF, comments and section order.
/// Repeated identity keys are normalized to one canonical key, never inherited
/// from an uploaded official ID. Repeated Metadata sections are ambiguous.
pub(crate) fn canonical_osu(bytes: &[u8], native_id: i64, set_id: i64) -> ApiResult<Vec<u8>> {
    if bytes.is_empty()
        || bytes.len() > MAX_OSU
        || !(2_000_000_000..=2_147_483_647).contains(&native_id)
        || !(2_000_000_000..=2_147_483_647).contains(&set_id)
    {
        return Err(invalid());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    if !text
        .trim_start_matches('\u{feff}')
        .starts_with("osu file format v")
    {
        return Err(invalid());
    }
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut output = String::with_capacity(text.len() + 64);
    let mut metadata = false;
    let mut seen_metadata = false;
    let mut seen_id = false;
    let mut seen_set = false;
    let append_missing = |output: &mut String, seen_id: bool, seen_set: bool| {
        if !seen_id || !seen_set {
            if !output.ends_with('\n') {
                output.push_str(newline);
            }
            if !seen_id {
                output.push_str(&format!("BeatmapID:{native_id}{newline}"));
            }
            if !seen_set {
                output.push_str(&format!("BeatmapSetID:{set_id}{newline}"));
            }
        }
    };
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\r', '\n']);
        let trimmed = content.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if metadata {
                append_missing(&mut output, seen_id, seen_set);
            }
            metadata = trimmed == "[Metadata]";
            if metadata && std::mem::replace(&mut seen_metadata, true) {
                return Err(invalid());
            }
        }
        let key = metadata
            .then(|| trimmed.split_once(':'))
            .flatten()
            .map(|(key, _)| key.trim());
        let replacement = match key {
            Some("BeatmapID") if !std::mem::replace(&mut seen_id, true) => {
                Some(format!("BeatmapID:{native_id}"))
            }
            Some("BeatmapSetID") if !std::mem::replace(&mut seen_set, true) => {
                Some(format!("BeatmapSetID:{set_id}"))
            }
            Some("BeatmapID" | "BeatmapSetID") => continue,
            _ => None,
        };
        if let Some(replacement) = replacement {
            output.push_str(&replacement);
            output.push_str(&line[content.len()..]);
        } else {
            output.push_str(line);
        }
    }
    if !seen_metadata {
        return Err(invalid());
    }
    if metadata {
        append_missing(&mut output, seen_id, seen_set);
    }
    let output = output.into_bytes();
    if output.len() > MAX_OSU {
        return Err(invalid());
    }
    // The existing bounded parser proves this remains a playable difficulty,
    // rather than merely validating the two substituted text fields.
    let parsed = maps::parse_map(output.clone()).map_err(|_| invalid())?;
    if parsed.official_id != Some(native_id) || parsed.official_set_id != Some(set_id) {
        return Err(invalid());
    }
    Ok(output)
}

struct SourceMember {
    id: i64,
    native_id: i64,
    native_set_id: i64,
    mode: i16,
    original_md5: String,
    bytes: Vec<u8>,
}

struct GeneratedMember {
    source: SourceMember,
    bytes: Vec<u8>,
    md5: String,
    sha256: String,
}

struct GeneratedPackage {
    original_sha256: String,
    original_byte_length: i64,
    members: Vec<GeneratedMember>,
    archive: Option<Vec<u8>>,
    retained_orphan_md5: Vec<String>,
}

fn derive_package(
    original_archive: Option<Vec<u8>>,
    members: Vec<SourceMember>,
    manifest: Vec<String>,
) -> ApiResult<GeneratedPackage> {
    if members.is_empty()
        || members.len() > 512
        || manifest.is_empty()
        || manifest.len() > 512
        || manifest.windows(2).any(|pair| pair[0] >= pair[1])
        || manifest.iter().any(|hash| {
            hash.len() != 32
                || !hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    {
        return Err(invalid());
    }
    let mut generated = Vec::with_capacity(members.len());
    for source in members {
        if format!("{:x}", md5::compute(&source.bytes)) != source.original_md5
            || manifest.binary_search(&source.original_md5).is_err()
            || maps::parse_map(source.bytes.clone())
                .map_err(|_| invalid())?
                .mode
                != source.mode
        {
            return Err(invalid());
        }
        let bytes = canonical_osu(&source.bytes, source.native_id, source.native_set_id)?;
        generated.push(GeneratedMember {
            md5: format!("{:x}", md5::compute(&bytes)),
            sha256: digest(&bytes),
            bytes,
            source,
        });
    }
    let mut derived_hashes = HashSet::new();
    if generated
        .iter()
        .any(|member| !derived_hashes.insert(&member.md5))
    {
        return Err(invalid());
    }
    let retained_orphan_md5 = manifest
        .iter()
        .filter(|hash| {
            !generated
                .iter()
                .any(|member| member.source.original_md5 == **hash)
        })
        .cloned()
        .collect::<Vec<_>>();
    let (original_sha256, original_byte_length, archive) = if let Some(original) = original_archive
    {
        let original_sha256 = digest(&original);
        let original_byte_length = original.len() as i64;
        let archive = derive_archive(&original, &generated, &manifest)?;
        (original_sha256, original_byte_length, Some(archive))
    } else {
        if generated.len() != 1 || !retained_orphan_md5.is_empty() {
            return Err(invalid());
        }
        let source = &generated[0].source.bytes;
        (digest(source), source.len() as i64, None)
    };
    Ok(GeneratedPackage {
        original_sha256,
        original_byte_length,
        members: generated,
        archive,
        retained_orphan_md5,
    })
}

fn derive_archive(
    original: &[u8],
    members: &[GeneratedMember],
    manifest: &[String],
) -> ApiResult<Vec<u8>> {
    let entries = maps::archive_names::preflight(original).map_err(|_| invalid())?;
    let mut archive = zip::ZipArchive::new(Cursor::new(original)).map_err(|_| invalid())?;
    if archive.len() != entries.len() {
        return Err(invalid());
    }
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let mut seen = HashSet::new();
    let mut expanded = 0_usize;
    for (index, metadata) in entries.into_iter().enumerate() {
        let mut entry = archive.by_index(index).map_err(|_| invalid())?;
        if entry.central_header_start() != metadata.central_start
            || entry.header_start() != metadata.header_start
            || entry.data_start() != metadata.data_start
            || entry.compressed_size() != metadata.compressed_size
            || entry.size() != metadata.size
            || entry.crc32() != metadata.crc32
        {
            return Err(invalid());
        }
        let limit = if metadata.name.to_ascii_lowercase().ends_with(".osu") {
            MAX_OSU
        } else {
            MAX_ARCHIVE
        };
        let mut contents = Vec::new();
        entry
            .by_ref()
            .take(limit as u64 + 1)
            .read_to_end(&mut contents)
            .map_err(|_| invalid())?;
        if contents.len() > limit || contents.len() as u64 != metadata.size {
            return Err(invalid());
        }
        expanded = expanded
            .checked_add(contents.len())
            .filter(|total| *total <= MAX_EXPANDED)
            .ok_or_else(invalid)?;
        let contents = if metadata.name.to_ascii_lowercase().ends_with(".osu") {
            let hash = format!("{:x}", md5::compute(&contents));
            if manifest.binary_search(&hash).is_err() || !seen.insert(hash.clone()) {
                return Err(invalid());
            }
            if let Some(member) = members
                .iter()
                .find(|member| member.source.original_md5 == hash)
            {
                if member.source.bytes != contents {
                    return Err(invalid());
                }
                &member.bytes
            } else {
                // A legacy hard-deleted difficulty has no current canonical
                // identity. Keep its authenticated original bytes and register
                // no alias rather than assigning a different map's native ID.
                maps::parse_map(contents.clone()).map_err(|_| invalid())?;
                &contents
            }
        } else {
            &contents
        };
        let mut options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        if let Some(time) = entry.last_modified() {
            options = options.last_modified_time(time);
        }
        if let Some(mode) = entry.unix_mode() {
            options = options.unix_permissions(mode);
        }
        if metadata.name.ends_with('/') {
            if !contents.is_empty() {
                return Err(invalid());
            }
            writer
                .add_directory(&metadata.name, options)
                .map_err(|_| invalid())?;
        } else {
            writer
                .start_file(&metadata.name, options)
                .map_err(|_| invalid())?;
            writer.write_all(contents).map_err(|_| invalid())?;
        }
    }
    if seen.len() != manifest.len() {
        return Err(invalid());
    }
    let bytes = writer.finish().map_err(|_| invalid())?.into_inner();
    if bytes.is_empty() || bytes.len() > MAX_ARCHIVE {
        return Err(invalid());
    }
    maps::archive_names::preflight(&bytes).map_err(|_| invalid())?;
    Ok(bytes)
}

/// File names are retained server records, never reflected request paths. Every
/// path component is checked before reading so a stored symlink cannot escape
/// the managed root. Source data is additionally authenticated by its hashes.
async fn read_stored(root: &Path, path: &str, limit: usize) -> ApiResult<Vec<u8>> {
    let relative = Path::new(path);
    if path.contains(['\\', ':'])
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(invalid());
    }
    let mut resolved = root.to_path_buf();
    for part in relative.components() {
        resolved.push(part);
        let metadata = tokio::fs::symlink_metadata(&resolved)
            .await
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    ApiError::missing()
                } else {
                    error.into()
                }
            })?;
        if metadata.file_type().is_symlink() {
            return Err(invalid());
        }
    }
    let file = tokio::fs::File::open(&resolved).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ApiError::missing()
        } else {
            error.into()
        }
    })?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes).await?;
    if bytes.is_empty() || bytes.len() > limit {
        return Err(invalid());
    }
    Ok(bytes)
}

async fn retained(state: &AppState, id: i64, archive: bool) -> ApiResult<Option<DistributedFile>> {
    let row = sqlx::query("SELECT d.native_set_id,d.kind,d.archive_path,d.sha256,d.byte_length,d.original_sha256,d.original_byte_length,p.archive_path AS original_archive_path,m.osu_path,m.derived_md5,m.sha256 AS osu_sha256,m.byte_length AS osu_length FROM local_beatmap_distribution_members m JOIN local_beatmap_distributions d ON d.id=m.distribution_id JOIN beatmap_packages p ON p.id=d.package_id WHERE m.beatmap_id=$1")
        .bind(id).fetch_optional(&state.db).await?;
    let Some(row) = row else { return Ok(None) };
    let set_id: i64 = row.get("native_set_id");
    beatmap_availability::require_active_set(&state.db, set_id).await?;
    let (path, sha256, byte_length, md5) = if archive {
        if row.get::<String, _>("kind") != "osz" {
            return Err(ApiError::missing());
        }
        // A derivative is never an alternative source after an operator has
        // removed the original archive. This also authenticates its provenance
        // on every archive delivery; individual .osu delivery stays independent.
        let original = read_stored(
            &state.maps_root,
            &row.get::<String, _>("original_archive_path"),
            MAX_ARCHIVE,
        )
        .await?;
        if digest(&original) != row.get::<String, _>("original_sha256")
            || original.len() as i64 != row.get::<i64, _>("original_byte_length")
        {
            return Err(invalid());
        }
        (
            row.get::<String, _>("archive_path"),
            row.get::<String, _>("sha256"),
            row.get::<i64, _>("byte_length"),
            None,
        )
    } else {
        (
            row.get::<String, _>("osu_path"),
            row.get::<String, _>("osu_sha256"),
            row.get::<i64, _>("osu_length"),
            Some(row.get::<String, _>("derived_md5")),
        )
    };
    let bytes = read_stored(
        &state.maps_root,
        &path,
        if archive { MAX_ARCHIVE } else { MAX_OSU },
    )
    .await?;
    if bytes.len() as i64 != byte_length
        || digest(&bytes) != sha256
        || md5
            .as_ref()
            .is_some_and(|expected| format!("{:x}", md5::compute(&bytes)) != *expected)
    {
        return Err(invalid());
    }
    if archive {
        maps::archive_names::preflight(&bytes).map_err(|_| invalid())?;
    }
    beatmap_availability::require_active_set(&state.db, set_id).await?;
    Ok(Some(DistributedFile { bytes, md5, sha256 }))
}

async fn write_owned(
    path: &Path,
    bytes: &[u8],
    owned: &mut Vec<std::path::PathBuf>,
) -> ApiResult<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    owned.push(path.to_path_buf());
    file.write_all(bytes).await?;
    file.sync_all().await?;
    Ok(())
}

pub(crate) async fn file(state: &AppState, id: i64, archive: bool) -> ApiResult<DistributedFile> {
    beatmap_availability::require_active_map(&state.db, id).await?;
    if let Some(file) = retained(state, id, archive).await? {
        return Ok(file);
    }
    let _permit = state.upload_permit()?;
    let package = sqlx::query("SELECT p.* FROM beatmaps b JOIN beatmap_packages p ON p.id=b.local_package_id WHERE b.id=$1 AND b.source='local' AND b.official_id IS NULL")
        .bind(id).fetch_optional(&state.db).await?.ok_or_else(ApiError::missing)?;
    let package_id: Uuid = package.get("id");
    let set_id: i64 = package.get("native_set_id");
    let kind: String = package.get("kind");
    if archive && kind != "osz" {
        return Err(ApiError::missing());
    }
    let rows = sqlx::query("SELECT id,native_id,native_set_id,mode,md5,osu_path,source,official_id FROM beatmaps WHERE local_package_id=$1 ORDER BY md5")
        .bind(package_id).fetch_all(&state.db).await?;
    let manifest: Vec<String> = package.get("member_md5");
    let actual: Vec<String> = rows.iter().map(|row| row.get("md5")).collect();
    if actual.is_empty()
        || actual
            .iter()
            .any(|hash| manifest.binary_search(hash).is_err())
        || rows.iter().any(|row| {
            row.get::<String, _>("source") != "local"
                || row.get::<Option<i64>, _>("official_id").is_some()
                || row.get::<i64, _>("native_set_id") != set_id
        })
    {
        return Err(invalid());
    }
    let mut members = Vec::with_capacity(rows.len());
    for row in rows {
        members.push(SourceMember {
            id: row.get("id"),
            native_id: row.get("native_id"),
            native_set_id: set_id,
            mode: row.get("mode"),
            original_md5: row.get("md5"),
            bytes: read_stored(&state.maps_root, &row.get::<String, _>("osu_path"), MAX_OSU)
                .await?,
        });
    }
    let original_archive = if kind == "osz" {
        Some(
            read_stored(
                &state.maps_root,
                &package.get::<String, _>("archive_path"),
                MAX_ARCHIVE,
            )
            .await?,
        )
    } else {
        None
    };
    let generated =
        tokio::task::spawn_blocking(move || derive_package(original_archive, members, manifest))
            .await
            .map_err(ApiError::internal)??;
    if !package.get::<bool, _>("legacy")
        && (package.get::<Option<String>, _>("sha256").as_ref() != Some(&generated.original_sha256)
            || package.get::<Option<i64>, _>("byte_length") != Some(generated.original_byte_length))
    {
        return Err(invalid());
    }
    let distribution_id = Uuid::new_v4();
    let prefix = format!("distribution-{distribution_id}");
    let directory = state.maps_root.join(&prefix);
    tokio::fs::create_dir(&directory).await?;
    let mut owned = Vec::new();
    let mut may_be_published = false;
    let result = async {
        for member in &generated.members {
            let path = directory.join(format!("{}.osu", member.md5));
            write_owned(&path, &member.bytes, &mut owned).await?;
        }
        let archive_path = generated.archive.as_ref().map(|_| format!("{prefix}/package.osz"));
        let archive_sha256 = generated.archive.as_ref().map(|bytes| digest(bytes));
        let archive_length = generated.archive.as_ref().map(|bytes| bytes.len() as i64);
        if let Some(bytes) = &generated.archive {
            let path = directory.join("package.osz");
            write_owned(&path, bytes, &mut owned).await?;
        }
        let mut tx = state.db.begin().await?;
        beatmap_availability::lock_active_set(&mut tx, set_id).await?;
        // A package fence serializes two derivatives of the same original. It
        // never precedes set availability, which is the withdrawal authority.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('illyaverse:local-distribution:'||$1::uuid::text,0))")
            .bind(package_id).execute(&mut *tx).await?;
        if sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM local_beatmap_distributions WHERE package_id=$1)")
            .bind(package_id).fetch_one(&mut *tx).await?
        {
            tx.rollback().await?;
            return Ok(());
        }
        let hashes: Vec<String> = generated.members.iter().flat_map(|member| [member.source.original_md5.clone(), member.md5.clone()])
            .chain(generated.retained_orphan_md5.iter().cloned()).collect();
        let keys = sqlx::query_scalar::<_, i64>("SELECT DISTINCT hashtextextended('illyaverse:local-map:'||value,0) AS key FROM unnest($1::text[]) AS hashes(value) ORDER BY key")
            .bind(hashes).fetch_all(&mut *tx).await?;
        for key in keys {
            sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(key).execute(&mut *tx).await?;
        }
        let current = sqlx::query("SELECT id,md5,native_id,native_set_id,mode,source FROM beatmaps WHERE local_package_id=$1 ORDER BY id FOR SHARE")
            .bind(package_id).fetch_all(&mut *tx).await?;
        if current.len()!=generated.members.len() || current.iter().any(|row| {
            !generated.members.iter().any(|member| member.source.id==row.get::<i64,_>("id")
                && member.source.original_md5==row.get::<String,_>("md5")
                && member.source.native_id==row.get::<i64,_>("native_id")
                && member.source.native_set_id==row.get::<i64,_>("native_set_id")
                && member.source.mode==row.get::<i16,_>("mode")
                && row.get::<String,_>("source")=="local")
        }) { return Err(invalid()); }
        sqlx::query("INSERT INTO local_beatmap_distributions(package_id,id,native_set_id,format_version,kind,original_sha256,original_byte_length,archive_path,sha256,byte_length,retained_orphan_md5) VALUES($1,$2,$3,1,$4,$5,$6,$7,$8,$9,$10)")
            .bind(package_id).bind(distribution_id).bind(set_id).bind(&kind).bind(&generated.original_sha256).bind(generated.original_byte_length)
            .bind(archive_path).bind(archive_sha256).bind(archive_length).bind(&generated.retained_orphan_md5).execute(&mut *tx).await?;
        for member in &generated.members {
            sqlx::query("INSERT INTO local_beatmap_distribution_members(distribution_id,beatmap_id,original_md5,derived_md5,native_id,native_set_id,osu_path,sha256,byte_length) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
                .bind(distribution_id).bind(member.source.id).bind(&member.source.original_md5).bind(&member.md5)
                .bind(member.source.native_id).bind(set_id).bind(format!("{prefix}/{}.osu",member.md5))
                .bind(&member.sha256).bind(member.bytes.len() as i64).execute(&mut *tx).await?;
        }
        may_be_published = true;
        match tx.commit().await {
            Ok(()) => Ok(()),
            Err(error) => {
                may_be_published = !maps::commit_definitely_rolled_back(&error);
                Err(ApiError::from(error))
            }
        }
    }.await;
    // Clean only exact paths created for this operation after a confirmed
    // rollback. An ambiguous COMMIT retains its immutable files for recovery.
    if !may_be_published {
        for path in owned {
            let _ = tokio::fs::remove_file(path).await;
        }
        let _ = tokio::fs::remove_dir(&directory).await;
    }
    result?;
    retained(state, id, archive).await?.ok_or_else(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn source() -> Vec<u8> {
        crate::maps::map_mutation_tests::osu_bytes("Canonical original")
    }

    fn derive_package(
        original: Option<Vec<u8>>,
        members: Vec<SourceMember>,
    ) -> ApiResult<GeneratedPackage> {
        let mut manifest = members
            .iter()
            .map(|member| member.original_md5.clone())
            .collect::<Vec<_>>();
        manifest.sort_unstable();
        super::derive_package(original, members, manifest)
    }

    #[test]
    fn canonical_transform_changes_only_metadata_identity() {
        let original = String::from_utf8(source()).unwrap().replace('\n', "\r\n");
        let original = format!("\u{feff}{original}").replace(
            "[Metadata]\r\n",
            "[Metadata]\r\n// Preserve attribution\r\nBeatmapID:123\r\nBeatmapSetID:456\r\n",
        );
        let derived = canonical_osu(original.as_bytes(), 2_000_000_013, 2_000_000_009).unwrap();
        assert_eq!(
            String::from_utf8(derived.clone()).unwrap(),
            original
                .replace("BeatmapID:123", "BeatmapID:2000000013")
                .replace("BeatmapSetID:456", "BeatmapSetID:2000000009")
        );
        assert_eq!(
            canonical_osu(&derived, 2_000_000_013, 2_000_000_009).unwrap(),
            derived
        );
        assert!(canonical_osu(original.as_bytes(), 123, 2_000_000_009).is_err());
        assert!(canonical_osu(original.as_bytes(), 2_000_000_013, 456).is_err());
    }

    #[test]
    fn missing_and_duplicate_identity_fields_have_one_canonical_value() {
        let original = String::from_utf8(source())
            .unwrap()
            .replace("[Metadata]\n", "[Metadata]\nBeatmapID:-1\nBeatmapID:42\n");
        let derived = String::from_utf8(
            canonical_osu(original.as_bytes(), 2_000_000_001, 2_000_000_002).unwrap(),
        )
        .unwrap();
        assert_eq!(derived.matches("BeatmapID:").count(), 1);
        assert_eq!(derived.matches("BeatmapSetID:").count(), 1);
        assert!(derived.contains("BeatmapID:2000000001\n"));
        assert!(derived.contains("BeatmapSetID:2000000002\n"));
        let ambiguous = original.replace("[HitObjects]", "[Metadata]\n[HitObjects]");
        assert!(canonical_osu(ambiguous.as_bytes(), 2_000_000_001, 2_000_000_002).is_err());
    }

    #[test]
    fn derived_archive_keeps_every_asset_name_and_bytes() {
        let original = source();
        let assets: BTreeMap<&str, &[u8]> = [
            ("曲/song.mp3", b"synthetic audio\0".as_slice()),
            ("背景.png", b"synthetic pixels".as_slice()),
            ("storyboard.osb", b"// attribution retained".as_slice()),
        ]
        .into();
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(
                "Original chart.osu",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        writer.write_all(&original).unwrap();
        for (name, bytes) in &assets {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        let package = writer.finish().unwrap().into_inner();
        let preserved = package.clone();
        let generated = derive_package(
            Some(package),
            vec![SourceMember {
                id: 1,
                native_id: 2_000_000_001,
                native_set_id: 2_000_000_002,
                mode: 0,
                original_md5: format!("{:x}", md5::compute(&original)),
                bytes: original.clone(),
            }],
        )
        .unwrap();
        assert_eq!(generated.original_sha256, digest(&preserved));
        let mut derived = zip::ZipArchive::new(Cursor::new(generated.archive.unwrap())).unwrap();
        assert_eq!(derived.len(), assets.len() + 1);
        for (name, bytes) in assets {
            let mut output = Vec::new();
            derived
                .by_name(name)
                .unwrap()
                .read_to_end(&mut output)
                .unwrap();
            assert_eq!(output, bytes);
        }
        let mut chart = Vec::new();
        derived
            .by_name("Original chart.osu")
            .unwrap()
            .read_to_end(&mut chart)
            .unwrap();
        assert_eq!(chart, generated.members[0].bytes);
        assert_ne!(chart, original);
        let mut original_zip = zip::ZipArchive::new(Cursor::new(preserved)).unwrap();
        let mut chart = Vec::new();
        original_zip
            .by_name("Original chart.osu")
            .unwrap()
            .read_to_end(&mut chart)
            .unwrap();
        assert_eq!(chart, original);
    }

    #[test]
    fn corruption_and_incomplete_original_archive_never_generate_aliases() {
        let original = source();
        let member = || SourceMember {
            id: 1,
            native_id: 2_000_000_001,
            native_set_id: 2_000_000_002,
            mode: 0,
            original_md5: format!("{:x}", md5::compute(&original)),
            bytes: original.clone(),
        };
        assert!(derive_package(Some(b"invalid zip".to_vec()), vec![member()]).is_err());
        let mut wrong = member();
        wrong.original_md5 = "a".repeat(32);
        assert!(derive_package(None, vec![wrong]).is_err());
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("audio.mp3", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"asset").unwrap();
        assert!(
            derive_package(Some(writer.finish().unwrap().into_inner()), vec![member()]).is_err()
        );
    }

    #[test]
    fn cp932_trail_backslash_asset_keeps_its_decoded_name() {
        let original = source();
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("chart.osu", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(&original).unwrap();
        writer
            .start_file("ab", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"retained CP932 asset bytes").unwrap();
        let mut package = writer.finish().unwrap().into_inner();
        let (local, central) = {
            let mut archive = zip::ZipArchive::new(Cursor::new(&package)).unwrap();
            let entry = archive.by_name("ab").unwrap();
            (
                entry.header_start() as usize + 30,
                entry.central_header_start() as usize + 46,
            )
        };
        // 表 is CP932 95 5C: the second byte is a multibyte trail, not a
        // directory separator. Both raw records must keep identical names.
        let name = [0x95, 0x5c];
        package[local..local + name.len()].copy_from_slice(&name);
        package[central..central + name.len()].copy_from_slice(&name);
        for flags_at in [local - 24, central - 38] {
            let flags = u16::from_le_bytes(package[flags_at..flags_at + 2].try_into().unwrap())
                & !(1 << 11);
            package[flags_at..flags_at + 2].copy_from_slice(&flags.to_le_bytes());
        }
        let generated = derive_package(
            Some(package),
            vec![SourceMember {
                id: 1,
                native_id: 2_000_000_001,
                native_set_id: 2_000_000_002,
                mode: 0,
                original_md5: format!("{:x}", md5::compute(&original)),
                bytes: original,
            }],
        )
        .unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(generated.archive.unwrap())).unwrap();
        let mut asset = Vec::new();
        archive
            .by_name("表")
            .unwrap()
            .read_to_end(&mut asset)
            .unwrap();
        assert_eq!(asset, b"retained CP932 asset bytes");
        assert_eq!(archive.len(), 2);
    }

    #[test]
    fn legacy_orphan_is_retained_only_under_the_exact_full_original_manifest() {
        let original = source();
        let orphan = crate::maps::map_mutation_tests::osu_bytes("Deleted original chart");
        let hash = format!("{:x}", md5::compute(&original));
        let orphan_hash = format!("{:x}", md5::compute(&orphan));
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in [
            ("current.osu", original.as_slice()),
            ("orphan.osu", orphan.as_slice()),
            ("asset.mp3", b"original asset".as_slice()),
        ] {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        let package = writer.finish().unwrap().into_inner();
        let source = || SourceMember {
            id: 1,
            native_id: 2_000_000_001,
            native_set_id: 2_000_000_002,
            mode: 0,
            original_md5: hash.clone(),
            bytes: original.clone(),
        };
        let mut manifest = vec![hash.clone(), orphan_hash.clone()];
        manifest.sort_unstable();
        let generated =
            super::derive_package(Some(package.clone()), vec![source()], manifest).unwrap();
        assert_eq!(generated.retained_orphan_md5, vec![orphan_hash]);
        assert_eq!(generated.members.len(), 1);
        let mut archive = zip::ZipArchive::new(Cursor::new(generated.archive.unwrap())).unwrap();
        let mut bytes = Vec::new();
        archive
            .by_name("orphan.osu")
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, orphan);
        bytes.clear();
        archive
            .by_name("current.osu")
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, generated.members[0].bytes);
        assert!(
            super::derive_package(Some(package), vec![source()], vec![hash]).is_err(),
            "an omitted original chart cannot become an unauthenticated orphan"
        );
    }
}
