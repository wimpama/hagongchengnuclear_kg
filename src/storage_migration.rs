//! Explicit copy-only migration of database-referenced local files to OSS.
use std::{collections::BTreeSet, path::Path, str::FromStr};

use anyhow::{bail, Context, Result};
use serde_json::Value;
use sqlx::{sqlite::SqliteConnectOptions, Row, SqlitePool};

use crate::{config::AppConfig, storage::BlobStore};

pub async fn run(arguments: &[String]) -> Result<()> {
    let mut apply = false;
    let mut preview = false;
    let mut env_file = None;
    let mut environment = crate::config::Environment::Development;
    let mut source_dir = None;
    let mut args = arguments.iter();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--apply" => apply = true,
            "--local-preview" => preview = true,
            "--env-file" => env_file = Some(args.next().context("--env-file requires a path")?),
            "--source-dir" => {
                source_dir = Some(args.next().context("--source-dir requires a path")?)
            }
            "--environment" => {
                environment = match args.next().map(String::as_str) {
                    Some("dev") => crate::config::Environment::Development,
                    Some("prod") => crate::config::Environment::Production,
                    _ => bail!("--environment must be dev or prod"),
                }
            }
            "--help" | "-h" => {
                println!("migrate-storage [--env-file .env] [--environment dev|prod] [--source-dir PATH] [--local-preview] [--apply]");
                println!("Default: inspect referenced files only. --apply copies and verifies; never deletes local files.");
                return Ok(());
            }
            _ => bail!("unknown migration option: {argument}"),
        }
    }
    if let Some(path) = env_file {
        load_env_file(Path::new(path))?;
    }
    if preview {
        if environment != crate::config::Environment::Development {
            bail!("--local-preview can only migrate the local development database");
        }
        let root = std::env::current_dir()?.join("data/local");
        let database = root
            .join("knowledge.sqlite")
            .to_string_lossy()
            .replace('\\', "/");
        std::env::set_var("KB_DATABASE_URL", format!("sqlite://{database}"));
        std::env::set_var("KB_DEV_DATABASE_URL", format!("sqlite://{database}"));
        std::env::set_var("KB_UPLOADS_DIR", root.join("uploads"));
    }
    let mut config = AppConfig::for_environment(environment)?;
    if let Some(source) = source_dir {
        match &mut config.storage {
            crate::config::StorageConfig::Oss { local_dir, .. } => *local_dir = source.into(),
            #[cfg(test)]
            _ => {}
        }
    }
    let options = SqliteConnectOptions::from_str(&config.database_url)?
        .read_only(true)
        .create_if_missing(false);
    let pool = SqlitePool::connect_with(options)
        .await
        .context("open existing SQLite read-only")?;
    let keys = referenced_keys(&pool).await?;
    let store = BlobStore::new(config.storage);
    let mut missing = 0;
    let mut total_bytes = 0u64;
    for key in &keys {
        super::storage::validate_key(key)
            .map_err(|_| anyhow::anyhow!("invalid referenced storage key"))?;
        match tokio::fs::metadata(store.local_dir().join(key)).await {
            Ok(metadata) if metadata.is_file() => total_bytes += metadata.len(),
            _ => missing += 1,
        }
    }
    println!(
        "Referenced files: {}; bytes: {total_bytes}; missing locally: {missing}",
        keys.len()
    );
    if missing > 0 {
        bail!("restore missing local originals before migration; no objects were uploaded");
    }
    if !apply {
        println!("Inspection complete. Stop this knowledge-service instance, then add --apply to copy to OSS.");
        return Ok(());
    }
    if !store.is_remote() {
        bail!("set KB_STORAGE_BACKEND=oss before applying migration");
    }
    let mut copied = 0;
    let mut existing = 0;
    for (index, key) in keys.iter().enumerate() {
        match store.migrate_file(key, content_type(key)).await {
            Ok(true) => copied += 1,
            Ok(false) => existing += 1,
            Err(error) => bail!(
                "migration stopped at file {}: {}; local originals retained",
                index + 1,
                error.message
            ),
        }
        println!(
            "Verified {}/{} (copied: {copied}, already identical: {existing})",
            index + 1,
            keys.len()
        );
    }
    println!("Migration complete. SQLite, Qdrant, publication state and local originals were not modified.");
    Ok(())
}

fn load_env_file(path: &Path) -> Result<()> {
    for (index, line) in std::fs::read_to_string(path)
        .context("read migration env file")?
        .lines()
        .enumerate()
    {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, value) = line
            .split_once('=')
            .with_context(|| format!("invalid env line {}", index + 1))?;
        let name = name.trim();
        if !name.starts_with("KB_") && !name.starts_with("OSS_") {
            continue;
        }
        if !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            bail!("invalid env variable on line {}", index + 1);
        }
        let mut value = value.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        // Read literal values only; never execute shell syntax or expand secrets.
        std::env::set_var(name, value);
    }
    Ok(())
}

async fn referenced_keys(pool: &SqlitePool) -> Result<BTreeSet<String>> {
    let mut keys = BTreeSet::new();
    let rows = sqlx::query("SELECT a.storage_key,p.media_storage_key,p.metadata_json FROM kb_assets a LEFT JOIN kb_asset_revisions r USING(asset_id) LEFT JOIN kb_content_parts p USING(revision_id) WHERE a.deleted_at IS NULL")
        .fetch_all(pool).await?;
    for row in rows {
        keys.insert(row.get("storage_key"));
        if let Some(key) = row.get::<Option<String>, _>("media_storage_key") {
            keys.insert(key);
        }
        if let Some(raw) = row.get::<Option<String>, _>("metadata_json") {
            let metadata: Value = serde_json::from_str(&raw)
                .context("invalid content metadata; migration aborted")?;
            for field in ["frameStorageKey", "sourceStorageKey"] {
                if let Some(key) = metadata.get(field).and_then(Value::as_str) {
                    keys.insert(key.to_owned());
                }
            }
        }
    }
    keys.extend(
        sqlx::query_scalar::<_, String>(
            "SELECT storage_key FROM kb_question_attachments WHERE expires_at>CURRENT_TIMESTAMP",
        )
        .fetch_all(pool)
        .await?,
    );
    Ok(keys)
}

fn content_type(key: &str) -> &'static str {
    match key.rsplit_once('.').map(|(_, ext)| ext) {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("mp4") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("avi") => "video/x-msvideo",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("m4a") => "audio/mp4",
        Some("ogg") => "audio/ogg",
        Some("flac") => "audio/flac",
        Some("pdf") => "application/pdf",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        Some("txt" | "md" | "csv") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migration_includes_revision_media_but_excludes_deleted_and_expired_files() {
        let state = crate::db::test_state().await;
        let pool = &state.db;
        sqlx::query(
            "INSERT INTO kb_course_nodes(node_id,course_id,level,title) VALUES('n','c',2,'node')",
        )
        .execute(pool)
        .await
        .unwrap();
        for (asset, deleted) in [("live", None), ("gone", Some("2000-01-01"))] {
            sqlx::query("INSERT INTO kb_assets(asset_id,node_id,asset_type,original_name,storage_key,byte_size,created_by,deleted_at) VALUES(?,'n','video','source',?,1,'admin',?)")
                .bind(asset).bind(format!("{asset}.mp4")).bind(deleted).execute(pool).await.unwrap();
            sqlx::query("INSERT INTO kb_asset_revisions(revision_id,asset_id,revision_number,source_sha256,parser_version,status,created_by) VALUES(?,?,1,'h','v1','review','admin')")
                .bind(asset).bind(asset).execute(pool).await.unwrap();
        }
        sqlx::query("INSERT INTO kb_content_parts(part_id,revision_id,part_index,modality,media_storage_key,metadata_json,content_sha256) VALUES('p','live',0,'video','clip.mp4',?, 'h')")
            .bind(r#"{"frameStorageKey":"frame.jpg","sourceStorageKey":"source.mp4"}"#).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO kb_conversations(conversation_id,user_id,course_id,scope_type,scope_id) VALUES('chat','student','c','course','c')").execute(pool).await.unwrap();
        for (id, expiry) in [("active", "2999-01-01"), ("expired", "2000-01-01")] {
            sqlx::query("INSERT INTO kb_question_attachments(attachment_id,conversation_id,user_id,original_name,mime_type,storage_key,byte_size,expires_at) VALUES(?,'chat','student','question','image/png',?,1,?)")
                .bind(id).bind(format!("{id}.png")).bind(expiry).execute(pool).await.unwrap();
        }
        assert_eq!(
            referenced_keys(pool).await.unwrap(),
            [
                "live.mp4",
                "clip.mp4",
                "frame.jpg",
                "source.mp4",
                "active.png"
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        );
    }
}
